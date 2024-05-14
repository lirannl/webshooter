use std::{
    borrow::Borrow,
    collections::HashMap,
    ops::{Deref, DerefMut},
    str::FromStr,
    sync::Arc,
};

use aes::Aes256;
use anyhow::Result;
use bytes::Bytes;
use http::{response::Parts, Response};
use lazy_static::lazy_static;
use rand::{rngs::ThreadRng, Rng};
use ring::signature::{self, Ed25519KeyPair};
use ring_compat::signature::ed25519;
use serde_json::from_slice;
use serde_yaml::from_value;
use tokio::sync::Mutex;
use warp::{
    filters::{header::header, path::path},
    reject::{reject, Rejection},
    reply::{self, reply, Reply},
    Filter,
};
use webshooter_shared::Bytes64;

use crate::{error::WebshooterError, get_config};

pub enum Session {
    Challenged(Vec<u8>),
    Approved,
}

lazy_static! {
    pub static ref SESSIONS: Mutex<HashMap<String, Session>> = Mutex::default();
}

pub async fn login() -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    let config = get_config().await;
    let challenge = path("challenge")
        .and(header::<Bytes64>("id"))
        .and_then(|id| async { Ok() });
    challenge
}

// pub async fn login(pubkey: impl Borrow<[u8]>) -> Result<(Response<()>, Vec<u8>)> {
//     let mut lock = SESSIONS.lock().await;
//     let session = lock
//         .get_mut(pubkey.borrow())
//         .ok_or(WebshooterError::NotChallenged)?;
//     todo!()
//     // Ok()
// }
// pub fn login() -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
//     let login = {

//     };
//     login
// }
