use anyhow::anyhow;
use bytes::Bytes;
use ecdsa::{elliptic_curve::pkcs8::DecodePublicKey, signature::Verifier};
use http::header::SET_COOKIE;
use lazy_static::lazy_static;
use p384::NistP384;
use rand::{thread_rng, Rng};
use serde::Deserialize;
use serde_json::from_slice;
use std::{collections::HashMap, ops::Deref, time::Duration};
use tokio::{sync::Mutex, time::timeout};
use ts_rs::TS;
use warp::{
    filters::{
        body, header::header, 
        method, path::{end, path}
    },
    reject::{self, Rejection},
    reply::{self, Reply},
    Filter,
};
use webshooter_shared::Bytes64;

use crate::{
    error::WebshooterError,
    get_config,
    ipc::{ipc_recv, ipc_send, IPCMessage},
    warp_ex::handle,
};

pub enum Session {
    Challenged(Vec<u8>),
    Approved { cookie: Vec<u8> },
}

lazy_static! {
    pub static ref SESSIONS: Mutex<HashMap<Vec<u8>, Session>> = Mutex::default();
}

const CHALLENGE_SIZE: usize = 256;

const COOKIE_SIZE: usize = 1024;

#[derive(Deserialize, TS)]
#[cfg_attr(debug, derive(Debug))]
#[ts(export)]
enum LoginParams {
    IdOnly { id: Bytes64 },
    Signature { id: Bytes64, signature: Bytes64 },
}

impl LoginParams {
    pub fn id(&self) -> &[u8] {
        match self {
            LoginParams::IdOnly { id } => id,
            LoginParams::Signature { id, .. } => id,
        }
    }
    pub fn into_id(self) -> Bytes64 {
        match self {
            LoginParams::IdOnly { id } => id,
            LoginParams::Signature { id, .. } => id.into(),
        }
    }
}

pub fn login() -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    let challenge = path("challenge")
        .and(end())
        .and(header("id"))
        .and(warp::get())
        .and_then(|id: Bytes64| {
            handle(async move {
                let mut challenge = [0 as u8; CHALLENGE_SIZE];
                thread_rng().try_fill(&mut challenge)?;
                {
                    let mut sessions = SESSIONS.lock().await;
                    sessions.insert(id.into(), Session::Challenged(challenge.to_vec()));
                }
                Ok(reply::Response::new(challenge.to_vec().into()))
            })
        });
    let login = path("login")
        .and(end())
        .and(method::post())
        .and(body::bytes())
        .and_then(|params: Bytes| {
            handle(async move {
                let config = get_config().await;
                let params = from_slice::<LoginParams>(&params)?;

                if let Some(_) = config.oidc {
                    Err(anyhow!("OIDC not yet supported."))?;
                } else {
                    let sessions = SESSIONS.lock().await;
                    let (id, signature) = match &params {
                        LoginParams::Signature { id, signature } => Ok((id, signature)),
                        _ => Err(WebshooterError::InvalidLogin),
                    }?;
                    if !config.authorised_keys.contains(&Bytes64::from(id.to_vec())) {
                        let mut sessions = sessions
                            .iter()
                            .filter_map(|(id, session)| match session {
                                Session::Challenged(_) => Some(id.to_owned()),
                                _ => None,
                            })
                            .collect::<Vec<_>>();
                        sessions.sort();
                        let timeout_secs = config.auth_timeout.unwrap_or(30);
                        timeout(
                            Duration::from_secs(timeout_secs),
                            async {loop {
                                let (message, mut connection) = ipc_recv().await?;
                                match (message, sessions.as_slice()) {
                                    (IPCMessage::Authorise(None), [_]) => {
                                        connection.write(&format!("Authorised {id}")).await?;
                                        break Ok::<_, anyhow::Error>(())    
                                    }
                                    (IPCMessage::Authorise(None), sessions) => {
                                        connection.write(&format!("Please select a session:\n{}", sessions.iter().enumerate().map(|(n, session_id)| {
                                            format!("{n}: {}", Bytes64::from(session_id.to_owned()))
                                        }).collect::<Vec<_>>().join("\n"))).await?; 
                                    }
                                    (message, sessions) if let IPCMessage::Authorise(Some(n)) = message => {
                                        if let Some((_, id)) = sessions.iter().enumerate().find(|(idx, session_id)| {
                                            *idx == n && id.iter().zip(session_id.iter()).all(|(id, session_id)| *id == *session_id)
                                        }) {
                                            connection.write(&format!("Authorised {}", Bytes64::from(id.to_owned()))).await?;
                                            break Ok(())
                                        }
                                        else {
                                            // Yield
                                            ipc_send(message, connection)?;
                                        }
                                    }
                                    (message, _) => {connection.write(&format!("Invalid message: {message:#?}")).await?;}
                                }
                            }})
                        .await
                        .unwrap_or_else(|_| {
                            Err(anyhow!("Did not get authorised within {timeout_secs} seconds, rejecting connection"))
                        })?;
                    }
                    let challenge = match sessions.get(id.deref()) {
                        Some(Session::Challenged(challenge)) => Ok(challenge),
                        _ => Err(WebshooterError::NotChallenged),
                    }?
                    .to_vec();
                    drop(sessions);

                    let key = ecdsa::VerifyingKey::<NistP384>::from_public_key_der(params.id());
                    let key = key?;
                    let verification =
                        key.verify(&challenge, &ecdsa::Signature::from_slice(&signature)?);
                    verification.map_err(|_| WebshooterError::ChallengeFailed)?;
                }
                
                let mut cookie = [0 as u8; COOKIE_SIZE];
                thread_rng().try_fill(&mut cookie)?;

                let mut sessions = SESSIONS.lock().await;
                sessions.insert(
                    params.into_id().into(),
                    Session::Approved {
                        cookie: cookie.to_vec(),
                    },
                );
                drop(sessions);

                Ok(warp::hyper::Response::builder()
                    .header(
                        SET_COOKIE.as_str(),
                        format!(
                            "token={}; HttpOnly; SameSite=Strict; Max-Age={}",
                            Bytes64::from(cookie.to_vec()),
                            config.session_ttl.as_secs()
                        ),
                    )
                    .body(warp::hyper::Body::empty())?)
            })
        });
    let check_auth = path("check_auth")
        .and(auth_validation())
        .map(|id| {
            let bytes = Bytes64::from(id);
            format!("Authenticated as {}", bytes)
        });
    challenge.or(login).or(check_auth)
}

#[derive(Debug)]
pub struct Unauthorized;

impl reject::Reject for Unauthorized {}

fn auth_validation() -> impl Filter<Extract = (Vec<u8>,), Error = Rejection> + Copy {
    warp::cookie("token")
    .or_else(|_| async { Err(Rejection::from(Unauthorized)) })
    .and_then(|token: Bytes64| async move {
        let sessions = SESSIONS.lock().await;
        let approved = sessions
            .values()
            .filter_map(|s| match s {
                Session::Approved { cookie } => Some(cookie.to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();
        drop(sessions);
        if let Some(id) = approved.iter().find(|t| *t == token.deref()) {
            Ok(id.clone())
        } else {
            Err(warp::reject::custom(Unauthorized))
        }
    })
}
