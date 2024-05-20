use data_encoding::BASE64;
use ecdsa::signature::Verifier;
use http::StatusCode;
use lazy_static::lazy_static;
use p384::pkcs8::DecodePublicKey;
use p384::NistP384;
use poem::web::Json;
use poem::{handler, FromRequest, Request, RequestBody};
use poem::{Error, IntoResponse, Response, Result};
use rand::{thread_rng, Rng};
use serde::Deserialize;
use std::collections::HashMap;
use std::ops::Deref;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::error::Elapsed;
use tokio::time::timeout;
use ts_rs::TS;
use webshooter_shared::Bytes64;

use crate::error::WebshooterError;
use crate::ipc::{ipc_recv, ipc_send, IPCMessage};
use crate::{get_config, update_config};

lazy_static! {
    pub static ref SESSIONS: Mutex<HashMap<Vec<u8>, Session>> = Mutex::default();
}

const CHALLENGE_SIZE: usize = 256;

const COOKIE_SIZE: usize = 1024;

struct Identity(Bytes64);

impl<'a> FromRequest<'a> for Identity {
    async fn from_request(req: &'a Request, _body: &mut RequestBody) -> Result<Self> {
        let identity = req
            .headers()
            .get("id")
            .and_then(|value| Bytes64::from_str(value.to_str().ok()?).ok())
            .ok_or(WebshooterError::NoAuthentication)?;
        Ok(Identity(identity))
    }
}

#[handler]
pub async fn check_identity(Identity(id): Identity) -> Result<impl IntoResponse> {
    if get_config().await.authorised_keys.contains(&id) {
        Ok(Response::default())
    } else {
        Err(WebshooterError::NotAuthorized.into())
    }
}

#[handler]
pub async fn get_challenge(Identity(id): Identity) -> Result<impl IntoResponse> {
    let _id = id.to_string();
    let mut challenge = [0 as u8; CHALLENGE_SIZE];
    thread_rng()
        .try_fill(&mut challenge)
        .map_err(|err| Error::new(err, StatusCode::INTERNAL_SERVER_ERROR))?;
    {
        let mut sessions = SESSIONS.lock().await;
        sessions.insert(id.into(), Session::Challenged(challenge.to_vec()));
    }
    poem::Result::Ok(Response::builder().body(challenge.to_vec()))
}

pub enum Session {
    Challenged(Vec<u8>),
    Approved { cookie: Vec<u8> },
}

#[derive(Deserialize, TS)]
#[cfg_attr(debug, derive(Debug))]
#[ts(export)]
enum LoginParams {
    IdOnly { id: Bytes64 },
    Signature { id: Bytes64, signature: Bytes64 },
}

impl LoginParams {
    pub fn id(&self) -> Bytes64<&[u8]> {
        match self {
            LoginParams::IdOnly { id } => Bytes64::from_bytes(&id[..]),
            LoginParams::Signature { id, .. } => Bytes64::from_bytes(&id[..]),
        }
    }
    pub fn into_id(self) -> Bytes64 {
        match self {
            LoginParams::IdOnly { id } => id,
            LoginParams::Signature { id, .. } => id.into(),
        }
    }
}

#[handler]
pub async fn login(Json(params): Json<LoginParams>) -> Result<impl IntoResponse> {
    let mut config = get_config().await;

    let (id, signature) = match &params {
        LoginParams::Signature { id, signature } => Ok((id, signature)),
        _ => Err(WebshooterError::InvalidLogin),
    }?;
    let id = id.to_owned();
    if !config
        .authorised_keys
        .contains(&Bytes64::from_bytes(id.to_vec()))
    {
        let sessions_lock = SESSIONS.lock().await;
        let mut sessions = sessions_lock
            .iter()
            .filter_map(|(id, session)| match session {
                Session::Challenged(_) => Some(id.to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();
        drop(sessions_lock);
        sessions.sort();
        let timeout_secs = config.auth_timeout.unwrap_or(30);
        timeout(Duration::from_secs(timeout_secs), async {
            loop {
                let (message, mut connection) = ipc_recv().await?;
                match (message, sessions.as_slice()) {
                    (IPCMessage::Authorise(None), [_]) => {
                        connection.write(&format!("Authorised {id}")).await?;
                        break Ok::<_, anyhow::Error>(());
                    }
                    (IPCMessage::Authorise(None), sessions) => {
                        connection
                            .write(&format!(
                                "Please select a session:\n{}",
                                sessions
                                    .iter()
                                    .enumerate()
                                    .map(|(n, session_id)| {
                                        format!(
                                            "{n}: {}",
                                            Bytes64::from_bytes(session_id.to_owned())
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            ))
                            .await?;
                    }
                    (message, sessions) if let IPCMessage::Authorise(Some(n)) = message => {
                        if let Some((_, id)) =
                            sessions.iter().enumerate().find(|(idx, session_id)| {
                                *idx == n
                                    && id
                                        .iter()
                                        .zip(session_id.iter())
                                        .all(|(id, session_id)| *id == *session_id)
                            })
                        {
                            connection
                                .write(&format!(
                                    "Authorised {}",
                                    Bytes64::from_bytes(id.to_owned())
                                ))
                                .await?;
                            break Ok(());
                        } else {
                            // Yield
                            ipc_send(message, connection)?;
                        }
                    }
                    (message, _) => {
                        connection
                            .write(&format!("Invalid message: {message:#?}"))
                            .await?;
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|err| Err(anyhow::Error::from(err)))
        .map_err(|err| match err.downcast::<Elapsed>() {
            Ok(elapsed) => Error::from_string(
                format!("Did not authorise within {}", elapsed),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            Err(err) => Error::from_string(err.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        })?;
    }
    let sessions = SESSIONS.lock().await;
    let challenge = match sessions.get(id.deref()) {
        Some(Session::Challenged(challenge)) => Ok(challenge),
        _ => Err(WebshooterError::NotChallenged),
    }?
    .to_vec();
    drop(sessions);

    let key = ecdsa::VerifyingKey::<NistP384>::from_public_key_der(&params.id())
        .map_err(|_| WebshooterError::InvalidLogin)?;
    let verification = key.verify(
        &challenge,
        &ecdsa::Signature::from_slice(&signature).map_err(|_| WebshooterError::InvalidLogin)?,
    );
    verification.map_err(|_| WebshooterError::ChallengeFailed)?;

    let mut cookie = [0 as u8; COOKIE_SIZE];
    thread_rng()
        .try_fill(&mut cookie)
        .map_err(|err| Error::new(err, StatusCode::INTERNAL_SERVER_ERROR))?;

    let mut sessions = SESSIONS.lock().await;
    sessions.insert(
        params.into_id().into(),
        Session::Approved {
            cookie: cookie.to_vec(),
        },
    );
    drop(sessions);
    config.authorised_keys.extend_one(id.clone());
    let _str = serde_json::to_string(&config).ok();
    update_config(config)
        .await
        .map_err(|err| Error::from_string(err.to_string(), StatusCode::INTERNAL_SERVER_ERROR))?;

    Ok(Response::builder()
        .header(
            "set-cookie",
            format!(
                "token={}; HttpOnly; Secure; SameSite=Strict",
                BASE64.encode(&cookie),
            ),
        )
        .finish())
}

pub struct Authenticated {
    pub id: Bytes64,
}

// Implements a token extractor
impl<'a> FromRequest<'a> for Authenticated {
    async fn from_request(req: &'a Request, _: &mut RequestBody) -> Result<Self> {
        let token = req
            .headers()
            .get_all("Cookie")
            .iter()
            .filter_map(|value| value.to_str().ok()?.split_once("="))
            .find_map(|(k, v)| if k == "token" { Some(v) } else { None })
            .ok_or(WebshooterError::NoAuthentication)?;
        let current_sessions = SESSIONS
            .lock()
            .await
            .iter()
            .filter_map(|(k, s)| match s {
                Session::Approved { cookie } => Some((
                    Bytes64::from_bytes(cookie.to_vec()).to_string(),
                    k.to_owned(),
                )),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let id = current_sessions
            .get(token)
            .ok_or(WebshooterError::NotAuthorized)?;

        Ok(Authenticated {
            id: Bytes64::from_bytes(id.clone()),
        })
    }
}
