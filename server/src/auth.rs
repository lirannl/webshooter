use crate::config::Bytes64;
use crate::error::WebshooterError;
use crate::ipc::{IPCMessage, ipc_recv, ipc_send};
use crate::{get_config, update_config};
use data_encoding::{BASE64, BASE64URL};
use ecdsa::signature::Verifier;
use p384::{NistP384, pkcs8::DecodePublicKey};
use salvo::http::{StatusCode as SalvoStatusCode, header::SET_COOKIE};
use salvo::{Request, Response, handler};
use std::collections::HashMap;
use std::ops::Deref;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::{error::Elapsed, timeout};

use rand::{RngExt, rng};
use serde::Deserialize;
use ts_rs::TS;

pub static AUTH_SESSIONS: LazyLock<Mutex<HashMap<Vec<u8>, Session>>> = Default::default();

/// SHA-256 (raw bytes) of the leaf TLS certificate, kept in sync with the live
/// [`salvo::conn::rustls::RustlsConfig`] by `cert_watch`. The WebTransport
/// negotiation endpoint hands this to clients so the browser can pin the
/// self-signed certificate. Must stay as raw bytes — the browser expects the
/// `serverCertificateHashes[].value` field to be the 32-byte digest.
pub static CERT_HASH: LazyLock<Mutex<Vec<u8>>> = LazyLock::new(|| Mutex::new(Vec::new()));

const CHALLENGE_SIZE: usize = 256;

const COOKIE_SIZE: usize = 1024;
const COOKIE_TTL: Duration = Duration::from_hours(24);

fn extract_identity(req: &Request) -> Result<Bytes64, WebshooterError> {
    req.headers()
        .get("id")
        .and_then(|value| value.to_str().ok())
        .and_then(|s| Bytes64::from_str(s).ok())
        .ok_or(WebshooterError::NoAuthentication)
}

#[handler]
pub async fn check_identity(req: &mut Request, res: &mut Response) {
    let id = match extract_identity(req) {
        Ok(id) => id,
        Err(_) => {
            res.status_code(SalvoStatusCode::UNAUTHORIZED);
            return;
        }
    };
    if get_config().await.authorised_keys.contains(&id) {
        res.status_code(SalvoStatusCode::OK);
    } else {
        res.status_code(SalvoStatusCode::FORBIDDEN);
    }
}

#[handler]
pub async fn get_challenge(req: &mut Request, res: &mut Response) {
    let id = match extract_identity(req) {
        Ok(id) => id,
        Err(_) => {
            res.status_code(SalvoStatusCode::UNAUTHORIZED);
            return;
        }
    };
    let _id = id.to_string();
    let mut challenge = [0_u8; CHALLENGE_SIZE];
    rng().fill(&mut challenge);
    {
        let mut sessions = AUTH_SESSIONS.lock().await;
        sessions.insert(id.into(), Session::Challenged(challenge.to_vec()));
    }
    res.status_code(SalvoStatusCode::OK);
    res.body(challenge.to_vec());
}

pub enum Session {
    Challenged(Vec<u8>),
    Approved {
        cookie: Vec<u8>,
        created_at: Instant,
    },
}

#[derive(Deserialize, TS)]
#[cfg_attr(feature = "debug", derive(Debug))]
#[ts(export)]
enum LoginParams {
    IdOnly {
        #[ts(type = "string")]
        id: Bytes64,
    },
    Signature {
        #[ts(type = "string")]
        id: Bytes64,
        #[ts(type = "string")]
        signature: Bytes64,
    },
}

impl LoginParams {
    pub fn id(&self) -> Bytes64<&[u8]> {
        match self {
            LoginParams::IdOnly { id } => Bytes64(&id[..]),
            LoginParams::Signature { id, .. } => Bytes64(&id[..]),
        }
    }
    pub fn into_id(self) -> Bytes64 {
        match self {
            LoginParams::IdOnly { id } => id,
            LoginParams::Signature { id, .. } => id,
        }
    }
}

pub async fn get_challenged_sessions() -> Vec<Vec<u8>> {
    let sessions_lock = AUTH_SESSIONS.lock().await;
    sessions_lock
        .iter()
        .filter_map(|(id, session)| match session {
            Session::Challenged(_) => Some(id.to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
}

const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

struct RateLimitEntry {
    count: u32,
    window_start: Instant,
}

static RATE_LIMITS: LazyLock<Mutex<HashMap<std::net::IpAddr, RateLimitEntry>>> = Default::default();

async fn check_rate_limit(ip: std::net::IpAddr, max_attempts: u32) -> bool {
    let mut limits = RATE_LIMITS.lock().await;
    let now = Instant::now();
    if let Some(entry) = limits.get_mut(&ip) {
        if now.duration_since(entry.window_start) > RATE_LIMIT_WINDOW {
            entry.count = 0;
            entry.window_start = now;
        }
        entry.count < max_attempts
    } else {
        true
    }
}

async fn record_rate_limit_failure(ip: std::net::IpAddr) {
    let mut limits = RATE_LIMITS.lock().await;
    let now = Instant::now();
    if let Some(entry) = limits.get_mut(&ip) {
        if now.duration_since(entry.window_start) > RATE_LIMIT_WINDOW {
            entry.count = 0;
            entry.window_start = now;
        }
        entry.count += 1;
    } else {
        limits.insert(
            ip,
            RateLimitEntry {
                count: 1,
                window_start: now,
            },
        );
    }
}

async fn reset_rate_limit(ip: std::net::IpAddr) {
    RATE_LIMITS.lock().await.remove(&ip);
}

fn client_ip(req: &Request) -> std::net::IpAddr {
    req.headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .and_then(|v| v.trim().parse().ok())
        .or_else(|| {
            req.headers()
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
        })
            .unwrap_or_else(|| {
            req.remote_addr()
                .ip()
                .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
        })
}

fn format_id(id: &dyn Deref<Target = [u8]>) -> String {
    Bytes64(&id[24..24 + 8])
        .to_string()
        .trim_matches('=')
        .to_string()
}

#[handler]
pub async fn login(req: &mut Request, res: &mut Response) {
    let params = match req.parse_json::<LoginParams>().await {
        Ok(params) => params,
        Err(err) => {
            res.status_code(SalvoStatusCode::BAD_REQUEST);
            res.render(format!("Invalid request body: {err}"));
            return;
        }
    };

    let ip = client_ip(req);
    let rate_limit = get_config().await.rate_limit.unwrap_or(10);
    if !check_rate_limit(ip, rate_limit).await {
        res.status_code(SalvoStatusCode::TOO_MANY_REQUESTS);
        res.render("Rate limit exceeded");
        return;
    }

    match login_inner(params).await {
        Ok(cookie) => {
            reset_rate_limit(ip).await;
            res.status_code(SalvoStatusCode::OK);
            if res
                .add_header(
                    SET_COOKIE,
                    format!(
                        "token={}; HttpOnly; Secure; SameSite=Strict",
                        BASE64.encode(&cookie),
                    ),
                    true,
                )
                .is_err()
            {
                res.status_code(SalvoStatusCode::INTERNAL_SERVER_ERROR);
                res.render(WebshooterError::InternalServerError.to_string());
                return;
            }
        }
        Err(err) => {
            let status = err.status();
            let msg = err.to_string();
            record_rate_limit_failure(ip).await;
            res.status_code(status);
            res.render(msg);
        }
    }
}

async fn login_inner(params: LoginParams) -> Result<Bytes64, WebshooterError> {
    let mut config = get_config().await;

    let (id, signature) = match &params {
        LoginParams::Signature { id, signature } => Ok((id, signature)),
        _ => Err(WebshooterError::InvalidLogin),
    }?;
    let id = id.to_owned();
    if !config.authorised_keys.contains(&Bytes64(id.to_vec())) {
        let timeout_secs = config.auth_timeout.unwrap_or(30);
        timeout(Duration::from_secs(timeout_secs), async {
            loop {
                let (message, mut connection) = ipc_recv().await?;
                match (message, get_challenged_sessions().await.as_slice()) {
                    (IPCMessage::Authorise(None), [_]) => {
                        connection
                            .write(&format!("Authorised {}", format_id(&id)))
                            .await?;
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
                                        format!("{n}: {}", format_id(session_id))
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
                                .write(&format!("Authorised {}", format_id(id)))
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
            Ok(_) => WebshooterError::NotAuthorized,
            Err(err) => WebshooterError::InvalidConfig(PathBuf::new(), err),
        })?;
    }
    let sessions = AUTH_SESSIONS.lock().await;
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
        &ecdsa::Signature::from_slice(signature).map_err(|_| WebshooterError::InvalidLogin)?,
    );
    verification.map_err(|_| WebshooterError::ChallengeFailed)?;

    let mut cookie = [0_u8; COOKIE_SIZE];
    rng().fill(&mut cookie);

    {
        let mut sessions = AUTH_SESSIONS.lock().await;
        sessions.insert(
            params.into_id().into(),
            Session::Approved {
                cookie: cookie.to_vec(),
                created_at: Instant::now(),
            },
        );
    }
    config.authorised_keys.extend_one(id.clone());
    let _str = serde_json::to_string(&config).ok();
    update_config(config)
        .await
        .map_err(|err| WebshooterError::InvalidConfig(PathBuf::new(), err))?;

    Ok(Bytes64(cookie.to_vec()))
}

pub struct Authenticated {
    #[allow(dead_code)]
    pub id: Bytes64,
}

impl Authenticated {
    async fn extract(req: &Request) -> Result<Self, WebshooterError> {
        let token = req
            .headers()
            .get_all("Cookie")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|cookie| cookie.trim().split_once("="))
            .find_map(|(k, v)| if k == "token" { Some(v) } else { None })
            .ok_or(WebshooterError::NoAuthentication)?;
        let current_sessions = AUTH_SESSIONS
            .lock()
            .await
            .iter()
            .filter_map(|(k, s)| match s {
                Session::Approved { cookie, created_at } => {
                    if created_at.elapsed() > COOKIE_TTL {
                        None
                    } else {
                        Some((Bytes64(cookie.to_vec()).to_string(), k.to_owned()))
                    }
                }
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let id = current_sessions
            .get(token)
            .ok_or(WebshooterError::NotAuthorized)?;

        Ok(Authenticated {
            id: Bytes64(id.clone()),
        })
    }
}

mod onetime {
    use rand::{RngExt, rng};
    use std::{collections::HashSet, ops::Deref, sync::LazyLock, time::Duration};
    use tokio::{spawn, sync::Mutex, time::sleep};

    use crate::{config::Bytes64, error::WebshooterError};

    #[cfg(debug_assertions)]
    const ONETIME_DURATION: Duration = Duration::from_mins(5);
    #[cfg(not(debug_assertions))]
    const ONETIME_DURATION: Duration = Duration::from_secs(5);
    const ONETIME_LENGTH: usize = 256;

    #[derive(Hash, PartialEq, Eq, Clone, Debug)]
    pub struct OnetimeToken([u8; ONETIME_LENGTH]);
    static ONETIME_AUTHORISATIONS: LazyLock<Mutex<HashSet<OnetimeToken>>> = Default::default();

    impl OnetimeToken {
        pub async fn new() -> Self {
            let mut token = [0; ONETIME_LENGTH];
            rng().fill(&mut token);
            let token = Self(token);
            ONETIME_AUTHORISATIONS.lock().await.insert(token.clone());
            {
                let token = token.clone();
                spawn(async move {
                    sleep(ONETIME_DURATION).await;
                    ONETIME_AUTHORISATIONS.lock().await.remove(&token);
                });
            }
            token
        }
        pub async fn check(self) -> bool {
            let hash_set = &mut ONETIME_AUTHORISATIONS.lock().await;
            hash_set.remove(&self)
        }
    }
    impl<B: Deref<Target = [u8]>> TryFrom<Bytes64<B>> for OnetimeToken {
        type Error = anyhow::Error;

        fn try_from(value: Bytes64<B>) -> Result<Self, Self::Error> {
            Ok(Self(
                *value.first_chunk().ok_or(WebshooterError::InvalidLogin)?,
            ))
        }
    }

    impl From<[u8; ONETIME_LENGTH]> for OnetimeToken {
        fn from(value: [u8; ONETIME_LENGTH]) -> Self {
            Self(value)
        }
    }

    impl Deref for OnetimeToken {
        type Target = [u8];

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
}

pub use onetime::OnetimeToken;

#[handler]
pub async fn negotiate_wt(req: &mut Request, res: &mut Response) {
    if Authenticated::extract(req).await.is_err() {
        res.status_code(SalvoStatusCode::FORBIDDEN);
        res.render(WebshooterError::NotAuthorized.to_string());
        return;
    }
    let token = OnetimeToken::new().await;
    let token = BASE64URL.encode(&token);
    let cert_hash = CERT_HASH.lock().await.clone();
    if res.add_header("token", token, true).is_err() {
        res.status_code(SalvoStatusCode::INTERNAL_SERVER_ERROR);
        res.render(WebshooterError::InternalServerError.to_string());
        return;
    }
    res.status_code(SalvoStatusCode::OK);
    res.body(cert_hash);
}

#[handler]
pub async fn check_auth(req: &mut Request, res: &mut Response) {
    if Authenticated::extract(req).await.is_err() {
        res.status_code(SalvoStatusCode::FORBIDDEN);
        res.render(WebshooterError::NotAuthorized.to_string());
        return;
    }
    res.status_code(SalvoStatusCode::OK);
    res.render("Authenticated");
}
