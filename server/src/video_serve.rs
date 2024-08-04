use std::{collections::HashMap, error::Error, sync::mpsc::RecvError};

use crate::{
    auth::{Session, SESSIONS},
    config::{Bytes64, Config},
    error::WebshooterError,
};
use anyhow::{bail, Result};
use ffmpeg_next::encoder::{self, Encoder};
use futures_util::future::join;
use scap::capturer::{self, Capturer, Options};
use tokio::fs;
use wtransport::{
    endpoint::SessionRequest,
    tls::{Certificate, CertificateChain, PrivateKey},
    Connection, Endpoint, Identity, ServerConfig, VarInt,
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
        .map(|c| c.to_string())
        .unwrap_or("".to_string());
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
        Ok(_) => Ok({
            let session = session.accept().await?;
            while let Ok(()) = session.send_datagram(b"Hello world!") {}
            session.close(VarInt::from_u32(0), b"Datagram test complete");
            // let fut = join(capture(&config, &session), setup_input(&config, &session)).await;
            // fut.0?;
            // fut.1?;
        }),
        Err(WebshooterError::NotAuthorized) => Ok(session.forbidden().await),
        Err(WebshooterError::NoAuthentication) => Ok(session.forbidden().await),
        Err(error) => {
            session.too_many_requests().await;
            Err(error.into())
        }
    }
}

pub async fn capture(config: &Config, session: &Connection) -> Result<()> {
    if !scap::has_permission() && !scap::request_permission() {
        bail!("Couldn't obtain screen capture permission")
    }

    let targets = scap::get_all_targets();

    let options = Options {
        fps: 60,
        target: None,
        show_cursor: true,
        show_highlight: true,
        excluded_targets: Some(
            targets
                .into_iter()
                .filter_map(|t| match t {
                    scap::Target::Window(_) => Some(t),
                    _ => None,
                })
                .collect(),
        ),
        output_type: scap::frame::FrameType::BGRAFrame,
        output_resolution: scap::capturer::Resolution::_720p,
        ..Default::default()
    };

    let mut capturer = Capturer::new(options);

    capturer.start_capture();

    let encoder = encoder::new();

    while let frame = capturer.get_next_frame() && let Some(frame) = match frame {
        Ok(frame) => Ok(Some(frame)),
        Err(err) => Err(err),
    }? {

    }

    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn setup_input(config: &Config, session: &Connection) -> Result<()> {
    use ashpd::desktop::remote_desktop::RemoteDesktop;

    let remote = RemoteDesktop::new().await?;

    Ok(())
}
