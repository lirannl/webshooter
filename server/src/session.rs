use aes::Aes256;
use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use http::{header::COOKIE, response::Parts};
use lazy_static::lazy_static;
use rand::{random, rngs::ThreadRng, thread_rng, Rng};
use ring::signature::{self, Ed25519KeyPair};
use ring_compat::signature::ed25519;
use serde_json::from_slice;
use serde_yaml::from_value;
use std::{
    borrow::Borrow,
    collections::HashMap,
    ops::{Deref, DerefMut},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::{sync::Mutex, time::timeout};
use warp::{
    filters::{body, header::header, path::path},
    reject::{reject, Rejection},
    reply::{self, reply, with_header, with_status, Reply, Response},
    Filter,
};
use webshooter_shared::Bytes64;

use crate::{error::WebshooterError, get_config, ipc::ipc_receiver, warp_ex::handle, APP_CONFIG};

pub enum Session {
    Challenged(Vec<u8>),
    Approved { cookie: Vec<u8> },
}

lazy_static! {
    pub static ref SESSIONS: Mutex<HashMap<Vec<u8>, Session>> = Mutex::default();
}

const CHALLENGE_SIZE: usize = 256;

const COOKIE_SIZE: usize = 1024;

pub async fn login() -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    let challenge = path("challenge")
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
        .and(header("id"))
        .and(
            body::bytes()
                .map(|b| Some(b))
                .or_else(|_| async { Ok::<_, Rejection>((None,)) }),
        )
        .and_then(|id: Bytes64, body| {
            handle(async move {
                let mut sessions = SESSIONS.lock().await;
                let config = get_config().await;

                if let Some(_) = config.oidc {
                    Err(anyhow!("OIDC not yet supported."))?;
                } else {
                    let challenge = sessions
                        .get(id.deref())
                        .ok_or(WebshooterError::NotChallenged)?;
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
                                    .any(|(idx, s_id)| idx as u32 == *n && id.deref() == s_id),
                                _ => false,
                            }),
                        )
                        .await
                        .map_err(|_| {
                            anyhow!("Did not get authorised within {timeout_secs} seconds, rejecting connection")
                        })??;
                    }
                    
                    todo!()
                }
                let mut cookie = [0 as u8; COOKIE_SIZE];
                thread_rng().try_fill(&mut cookie)?;
                sessions.insert(
                    id.into(),
                    Session::Approved {
                        cookie: cookie.to_vec(),
                    },
                );

                Ok(warp::hyper::Response::builder()
                    .header(COOKIE.as_str(), cookie.to_vec())
                    .body(warp::hyper::Body::empty())?)
            })
        });
    challenge
}
