use std::{str::FromStr, time::Duration};

use crate::{
    auth::OnetimeToken,
    config::{Bytes64, Config},
    error::WebshooterError,
    logging::log,
};
use anyhow::{anyhow, bail, Result};
use futures_util::TryFutureExt;
use tokio::time::sleep;
use wtransport::{endpoint::SessionRequest, Connection, Endpoint, Identity, ServerConfig, VarInt};

pub async fn setup_wt(config: Config, identity: Identity) -> Result<()> {
    let server_config = ServerConfig::builder()
        .with_bind_default(config.http_config.port)
        .with_identity(&identity)
        .keep_alive_interval(Some(Duration::from_mins(5)))
        .build();

    let server = Endpoint::server(server_config)?;

    for id in 0.. {
        let session = server.accept().await;
        let config = config.clone();
        tokio::spawn(async move {
            (async move || -> Result<()> {
                let request = session.await?;
                let token = request
                    .path()
                    .split_once("?")
                    .map(|(_, params)| params.split("&"))
                    .and_then(|params| {
                        params
                            .filter_map(|param| param.split_once("="))
                            .find_map(|(k, v)| if k == "token" { Some(v) } else { None })
                    })
                    .ok_or(WebshooterError::NoAuthentication)?;
                let token = Bytes64::from_str(token)?;
                if OnetimeToken::try_from(token)?.check().await {
                    handle_wt_connection(&config, request.accept().await?)
                        .await
                        .map_err(|err| {
                            anyhow!("Error during webtransport connection {id}: {err:#?}")
                        })
                } else {
                    request.forbidden().await;
                    Err(WebshooterError::NotAuthorized.into())
                }
            })()
            .await
            .unwrap_or_else(log)
        });
    }
    Ok(())
}

pub async fn handle_wt_connection(config: &Config, connection: Connection) -> Result<()> {
    loop {
     if let Ok(_) = connection.receive_datagram().await {
        for i in 1..1000 {
            connection.send_datagram(format!("{i}").as_bytes())?;
        }
     }
    }
}

// pub async fn capture(config: &Config, session: &Connection) -> Result<()> {
//     if !scap::has_permission() && !scap::request_permission() {
//         bail!("Couldn't obtain screen capture permission")
//     }

//     let targets = scap::get_all_targets();

//     let options = Options {
//         fps: 60,
//         target: None,
//         show_cursor: true,
//         show_highlight: true,
//         excluded_targets: Some(
//             targets
//                 .into_iter()
//                 .filter_map(|t| match t {
//                     scap::Target::Window(_) => Some(t),
//                     _ => None,
//                 })
//                 .collect(),
//         ),
//         output_type: scap::frame::FrameType::BGRAFrame,
//         output_resolution: scap::capturer::Resolution::_720p,
//         ..Default::default()
//     };

//     let mut capturer = Capturer::new(options);

//     capturer.start_capture();

//     let encoder = encoder::new();

//     while let frame = capturer.get_next_frame()
//         && let Some(frame) = match frame {
//             Ok(frame) => Ok(Some(frame)),
//             Err(err) => Err(err),
//         }?
//     {}

//     Ok(())
// }

// #[cfg(target_os = "linux")]
// pub async fn setup_input(config: &Config, session: &Connection) -> Result<()> {
//     use ashpd::desktop::remote_desktop::RemoteDesktop;

//     let remote = RemoteDesktop::new().await?;

//     Ok(())
// }
