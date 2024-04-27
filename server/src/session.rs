use std::{borrow::Borrow, collections::HashMap};

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine};
use http::Response;
use lazy_static::lazy_static;
use rand::{rngs::ThreadRng, Rng};
use tokio::sync::Mutex;
use webshooter_shared::BytesLowercase;

use crate::error::WebshooterError;

pub enum Session {
    Challenged(Vec<u8>),
    PendingAuth {
        symmetric_key: Vec<u8>,
        challenge: Vec<u8>,
    },
    Active {
        symmetric_key: Vec<u8>,
        challenge: Vec<u8>,
    },
}

lazy_static! {
    pub static ref SESSIONS: Mutex<HashMap<Vec<u8>, Session>> = Mutex::default();
}

pub async fn get_challenge(pubkey: impl Into<Vec<u8>>) -> Result<(Response<()>, Vec<u8>)> {
    let mut challenge = [0 as u8; 256];
    ThreadRng::default().fill(&mut challenge);
    let challenge = challenge.to_vec();

    let mut lock = SESSIONS.lock().await;
    let entry = lock.entry(pubkey.into());
    entry
        .and_modify(|s| match s {
            Session::Challenged(chl) => *chl = challenge.clone(),
            Session::Active { challenge: chl, .. } => *chl = challenge.clone(),
            Session::PendingAuth { .. } => *s = Session::Challenged(challenge.clone()),
        })
        .or_insert(Session::Challenged(challenge.clone()));

    Ok((Response::builder().body(())?, challenge))
}

pub async fn login(pubkey: impl Borrow<[u8]>) -> Result<(Response<()>, Vec<u8>)> {
    let mut lock = SESSIONS.lock().await;
    let session = lock
        .get_mut(pubkey.borrow())
        .ok_or(WebshooterError::NotChallenged)?;
    todo!()
    // Ok()
}
/*pub fn login() -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    let login = {
        let pubkey_extractor = header::<String>("pubkey")
            .and_then(|pubkey| async { STANDARD.decode(pubkey).map_err(|_err| reject()) });
        let login = warp::path("login")
            .and(pubkey_extractor.clone())
            .and_then(|pubkey| async {
                handle(
                    (async move || {
                        let mut challenge = [0 as u8; 256];
                        ThreadRng::default().fill(&mut challenge);
                        let challenge = challenge.to_vec();

                        let mut lock = SESSIONS.lock().await;
                        let entry = lock.entry(pubkey);
                        entry
                            .and_modify(|s| match s {
                                Session::Challenged(s) => *s = challenge.clone(),
                                Session::Approved { challenge: s, .. } => {
                                    *s = Some(challenge.clone())
                                }
                            })
                            .or_insert(Session::Challenged(challenge.clone()));

                        Ok(Box::new(challenge.to_vec()))
                    })()
                    .await,
                )
            });
        let challenge = warp::path("login/challenge")
            .and(pubkey_extractor)
            .and_then(|pubkey| async {
                handle(
                    (async move || {
                        let mut lock = SESSIONS.lock().await;
                        let session = lock.get_mut(&pubkey).ok_or(WebshooterError::NotChallenged)?;
                        sessio
                        Ok()
                    })()
                    .await,
                )
            });
        login.or(challenge)
    };
    login
}*/
