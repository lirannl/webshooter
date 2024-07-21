use std::collections::HashMap;

use crate::{
    auth::{Session, SESSIONS},
    config::{Bytes64, Config},
    error::WebshooterError,
};
use anyhow::Result;
use futures_util::{future::join, FutureExt};
use tokio::fs;
use wtransport::{
    endpoint::SessionRequest,
    tls::{Certificate, CertificateChain, PrivateKey},
    Connection, Endpoint, Identity, ServerConfig,
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
            let fut = join(capture(&config, &session), setup_input(&config, &session)).await;
            fut.0?;
            fut.1?;
        }),
        Err(WebshooterError::NotAuthorized) => Ok(session.forbidden().await),
        Err(WebshooterError::NoAuthentication) => Ok(session.forbidden().await),
        Err(error) => {
            session.too_many_requests().await;
            Err(error.into())
        }
    }
}

#[cfg(target_os = "linux")]
pub async fn capture(config: &Config, session: &Connection) -> Result<()> {
    use ashpd::{
        desktop::{
            screencast::{CursorMode, Screencast, SourceType, Stream},
            Request,
        },
        enumflags2::BitFlags,
    };
    use ffmpeg_next::{format::Pixel, Dictionary, Rational};

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
    update_config(config).await?;

    let streams = streams.streams().to_owned();
    let stream = streams.first().ok_or(WebshooterError::CaptureFailed)?;
    let fd = cast.open_pipe_wire_remote(&cast_session).await?;
    let encoder = ffmpeg_next::encoder::new();
    let mut video = encoder.video()?;
    video.set_bit_rate(5000000);
    let framerate = Rational::new(30, 1);
    video.set_frame_rate(Some(framerate));
    let (width, height) = stream.size().ok_or(WebshooterError::CaptureFailed)?;
    video.set_aspect_ratio(Rational::new(width, height).reduce());
    let (width, height) = (width.abs() as u32, height.abs() as u32);
    video.set_width(width);
    video.set_height(height);
    video.set_format(Pixel::ABGR);
    video.set_time_base(framerate.invert());
    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn setup_input(config: &Config, session: &Connection) -> Result<()> {
    use ashpd::desktop::remote_desktop::RemoteDesktop;

    let remote = RemoteDesktop::new().await?;
    

    Ok(())
}
