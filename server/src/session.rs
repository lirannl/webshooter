use anyhow::anyhow;
use http::header::SET_COOKIE;
use lazy_static::lazy_static;
use rand::{thread_rng, Rng};
use ring::signature::{VerificationAlgorithm, ED25519};
use serde::Deserialize;
use untrusted::Input;
use std::{
    collections::HashMap,
    ops::Deref,
    time::Duration,
};
use tokio::{sync::Mutex, time::timeout};
use warp::{
    filters::{body, header::header, method::{self}, path::{end, path}},
    reject::{self, Rejection},
    reply::{self, Reply},
    Filter,
};
use webshooter_shared::Bytes64;

use crate::{error::WebshooterError, get_config, ipc::ipc_receiver, warp_ex::handle};

pub enum Session {
    Challenged(Vec<u8>),
    Approved { cookie: Vec<u8> },
}

lazy_static! {
    pub static ref SESSIONS: Mutex<HashMap<Vec<u8>, Session>> = Mutex::default();
}

const CHALLENGE_SIZE: usize = 256;

const COOKIE_SIZE: usize = 1024;

#[derive(Deserialize)]
enum LoginParams {
    IdOnly(Bytes64),
    Signature {
        id: Bytes64,
        signature: Bytes64
    },
}

impl LoginParams {
    pub fn id(&self) -> &Bytes64 {
        match self {
            LoginParams::IdOnly(id) => id,
            LoginParams::Signature { id, .. } => id,
        }
    }
    pub fn into_id(self) -> Bytes64 {
        match self {
            LoginParams::IdOnly(id) => id,
            LoginParams::Signature { id, .. } => id,
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
    let login = path("login").and(end())
        .and(method::post())
        .and(body::form())
        .and_then(|form: LoginParams| {
            handle(async move {
                let config = get_config().await;
                
                if let Some(_) = config.oidc {
                    Err(anyhow!("OIDC not yet supported."))?;
                } else {
                    let sessions = SESSIONS.lock().await;
                    let (id, signature) = match &form {
                        LoginParams::Signature {id, signature} => Ok((id, signature)),
                        _ => Err(WebshooterError::InvalidLogin)
                    }?;
                    if !config.authorised_keys.contains(&id) {
                        let mut sessions = sessions
                            .iter()
                            .filter_map(|(id, session)| match session {
                                Session::Challenged(_) => Some(id.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>();
                        sessions.sort();
                        let timeout_secs = config.auth_timeout.unwrap_or(30);
                        timeout(
                            Duration::from_secs(timeout_secs),
                            ipc_receiver().wait_for(|msg| match msg {
                                crate::ipc::IPCMessage::AuthoriseN(n) => sessions
                                    .iter()
                                    .enumerate()
                                    .any(|(idx, s_id)| idx as u32 == *n && form.id().deref() == s_id),
                                _ => false,
                            }),
                        )
                        .await
                        .map_err(|_| {
                            anyhow!("Did not get authorised within {timeout_secs} seconds, rejecting connection")
                        })??;
                    }
                    let challenge = match sessions.get(id.deref()) {
                        Some(Session::Challenged(challenge)) => Ok(challenge),
                        _ => Err(WebshooterError::NotChallenged)
                    }?.to_vec();
                    drop(sessions);
                    
                    let verification = ED25519.verify(Input::from(&form.id()), Input::from(&challenge), Input::from(&signature));
                    verification.map_err(|_| WebshooterError::ChallengeFailed)?;
                }
                let mut cookie = [0 as u8; COOKIE_SIZE];
                thread_rng().try_fill(&mut cookie)?;

                let mut sessions = SESSIONS.lock().await;
                sessions.insert(
                    form.into_id().into(),
                    Session::Approved {
                        cookie: cookie.to_vec(),
                    },
                );
                drop(sessions);

                Ok(warp::hyper::Response::builder()
                    .header(SET_COOKIE.as_str(), format!("token={}; HttpOnly; SameSite=Strict; Max-Age={}", Bytes64::from(cookie.to_vec()), config.session_ttl.as_secs()))
                    .body(warp::hyper::Body::empty())?)
            })
        });
    let check_auth = path("check_auth").and(end()).and(auth_validation()).map(|id| format!("Authenticated as {}", Bytes64::from(id)));
    challenge.or(login).or(check_auth)
}

#[derive(Debug)]
pub struct Unauthorized;

impl reject::Reject for Unauthorized {}

fn auth_validation() -> impl Filter<Extract = (Vec<u8>,), Error = Rejection> + Copy {
    warp::cookie("token").and_then(|token: Bytes64| async move {
        let sessions = SESSIONS.lock().await;
        let approved = sessions.values()
            .filter_map(|s| match s {Session::Approved { cookie } => Some(cookie.to_owned()), _ => None}).collect::<Vec<_>>();
        drop(sessions);
        if let Some(id) = approved.iter().find(|t| *t == token.deref()) {
            Ok(id.clone())
        } else {
            Err(warp::reject::custom(Unauthorized))
        }
    })
}