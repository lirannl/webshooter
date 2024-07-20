use std::collections::HashMap;

use crate::{
    auth::{Session, SESSIONS},
    config::{Bytes64, Config},
    error::WebshooterError,
};
use anyhow::Result;
#[cfg(target_os = "linux")]
use ashpd::{
    desktop::{
        remote_desktop::RemoteDesktop,
        screencast::{CursorMode, Screencast, SourceType, Stream},
    },
    enumflags2::BitFlags,
};
use futures_util::FutureExt;
use tokio::fs;
use wtransport::{
    endpoint::SessionRequest,
    tls::{Certificate, CertificateChain, PrivateKey},
    Endpoint, Identity, ServerConfig,
};

pub async fn setup_wt(config: Config) -> Result<()> {
    let cert = Certificate::from_der(fs::read(&config.http_config.ssl_conf.certificate).await?)?;
    let key = PrivateKey::from_der_pkcs8(fs::read(&config.http_config.ssl_conf.key).await?);
    let identity = Identity::new(CertificateChain::single(cert), key);

    let server_config = ServerConfig::builder()
        .with_bind_address((&config.http_config).into())
        .with_identity(&identity)
        .build();

    let server = Endpoint::server(server_config)?;

    loop {
        let session = server.accept().await.await?;
        let _ = handle_wt_connection(&config, session).await;
    }
}

pub async fn handle_wt_connection(config: &Config, session: SessionRequest) -> Result<()> {
    let cookie = session
        .headers()
        .get("Cookie")
        .map(|c| c.to_string()).unwrap_or("".to_string());
    let token = cookie
        .split(";")
        .filter_map(|cookie| cookie.trim().split_once("="))
        .find_map(|(k, v)| if k == "token" { Some(v) } else { None })
        .ok_or(WebshooterError::NoAuthentication);
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
    match token.and_then(|token| {
        current_sessions
            .get(token)
            .ok_or(WebshooterError::NotAuthorized)
    }) {
        Ok(_) => Ok(capture(&config, session).await?),
        Err(WebshooterError::NotAuthorized) => Ok(session.forbidden().await),
        Err(WebshooterError::NoAuthentication) => Ok(session.forbidden().await),
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "linux")]
pub async fn capture(config: &Config, session: SessionRequest) -> Result<()> {
    use ashpd::desktop::Request;
    use futures_util::stream;
    use wtransport::endpoint::SessionRequest;

    use crate::update_config;
    let cast = Screencast::new().await?;
    let cast_session = cast.create_session().await?;
    cast.select_sources(
        &cast_session,
        CursorMode::Embedded,
        BitFlags::from_flag(config.capture_type.clone().into()),
        false,
        config.pipewire_key.as_deref(),
        ashpd::desktop::PersistMode::ExplicitlyRevoked,
    )
    .await?;
    let streams = Request::response(
        &cast
            .start(&cast_session, &ashpd::WindowIdentifier::None)
            .await?,
    )?;
    let mut config = config.clone();
    config.pipewire_key = streams.restore_token().map(|key| key.to_string());
    update_config(config);
    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn setup_input(config: &Config) -> Result<()> {
    let remote = RemoteDesktop::new().await?;

    Ok(())
}
