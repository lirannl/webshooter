use std::{borrow::Borrow, collections::hash_map::Entry};

use base64::{engine::general_purpose::STANDARD as BASE64ENGINE, Engine};
use poem::{handler, http::StatusCode, Error, FromRequest, Request, Result};
use rand::{rngs::ThreadRng, Rng};
use ring::{rsa::{self, KeyPairComponents, PublicKeyComponents}, signature::RSA_PKCS1_SHA256};

use crate::{error::WebshooterError, ACTIVE_SESSIONS};

pub enum Session {
    Challenged(Vec<u8>),
    Approved {
        symmetric_key: Vec<u8>,
        challeng: Option<Vec<u8>>,
    },
}

struct PubKey(Vec<u8>);

impl<'a> FromRequest<'a> for PubKey {
    async fn from_request(req: &'a Request, _: &mut poem::RequestBody) -> Result<Self> {
        let header = req
            .headers()
            .iter()
            .find_map(|(name, value)| {
                let name = &name.as_str();
                if name.to_lowercase() == "pubkey"
                    || name.to_lowercase() == "publickey"
                    || name.to_lowercase() == "pkey"
                {
                    Some(value)
                } else {
                    None
                }
            })
            .and_then(|value| BASE64ENGINE.decode(value.to_str().ok()?).ok())
            .ok_or(Error::from_string(
                "Missing or invalid pubkey header. Please provide a base64 string",
                StatusCode::BAD_REQUEST,
            ))?;
        Ok(PubKey(header))
    }
}

#[handler]
pub async fn login(pubkey: PubKey) -> Vec<u8> {
    let pubkey = pubkey.0;

    let mut challeng = [0 as u8; 256];
    ThreadRng::default().fill(&mut challeng);
    let challeng = challeng.to_vec();

    let mut lock = ACTIVE_SESSIONS.lock().await;
    let mut lock = lock.entry(pubkey);
    lock.and_modify(|s| match s {
        Session::Challenged(s) => *s = challeng.clone(),
        Session::Approved { challeng: s, .. } => *s = Some(challeng.clone()),
    })
    .or_insert(Session::Challenged(challeng.clone()));

    challeng
}

#[handler]
pub async fn challenge(pubkey: PubKey) -> Result<(), anyhow::Error> {
    let key = pubkey.0;

    let challeng = ACTIVE_SESSIONS
        .lock()
        .await
        .get_mut(&key)
        .and_then(|session| match session {
            Session::Approved { challeng, .. } => challeng.as_ref(),
            Session::Challenged(challeng) => Some(challeng),
        })
        .ok_or(WebshooterError::InvalidRequest(
            "You have not been challenged yet, please attempt a login first".to_string(),
        ))?;

    // let pubkey = rsa::KeyPair::from_components();

    Ok(())
}
