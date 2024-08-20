use std::{
    io::Read,
    os::fd::{FromRawFd, IntoRawFd},
    str::FromStr,
    time::Duration,
};

use crate::{
    auth::OnetimeToken,
    config::{Bytes64, Config},
    error::WebshooterError,
    get_config,
    logging::log,
    update_config,
};
use anyhow::{anyhow, Result};
use ashpd::{
    desktop::{
        screencast::{self},
        PersistMode,
    },
    enumflags2::BitFlags,
    WindowIdentifier,
};
use bytes::Buf;
use futures_util::TryFutureExt;
use scap::capturer::{Capturer, Options};
use tokio::{
    fs::File,
    io::{AsyncReadExt, BufReader},
};
use wtransport::{endpoint::IncomingSession, Connection, Endpoint, Identity, ServerConfig};

pub async fn setup_wt(config: Config, identity: Identity) -> Result<()> {
    let server_config = ServerConfig::builder()
        .with_bind_default(config.http_config.port)
        .with_identity(&identity)
        .keep_alive_interval(Some(Duration::from_mins(5)))
        .build();

    let server = Endpoint::server(server_config)?;

    for id in 0.. {
        let session = server.accept().await;
        tokio::spawn(async move {
            webtransport_auth(session)
                .and_then(handle_wt_connection)
                .await
                .map_err(|err| anyhow!("Error during webtransport connection {id}: {err:#?}"))
                .unwrap_or_else(log)
        });
    }
    Ok(())
}

async fn webtransport_auth(session: IncomingSession) -> Result<Connection> {
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
        let connection = request.accept().await?;
        Ok(connection)
    } else {
        request.forbidden().await;
        Err(WebshooterError::NotAuthorized.into())
    }
}

pub async fn handle_wt_connection(connection: Connection) -> Result<()> {
    let mut capturer = Capturer::new(Options {
        ..Default::default()
    });
    capturer.start_capture();
    let mut buf = [0; 512];
    let mut vec = Vec::new();
    for _ in 0..10 {
        let frame = capturer.get_next_frame()?;
        let mut frame = match frame {
            scap::frame::Frame::BGRx(frame) => Ok(frame),
            _ => Err(WebshooterError::InvalidLogin),
        }?;
        vec.append(&mut frame.data);
    }
    let mut reader = BufReader::new(vec.as_slice());
    if let Ok(_) = connection.receive_datagram().await {
        while let n = reader.read(&mut buf[..]).await?
            && n > 0
        {
            connection.send_datagram(&buf[0..(n - 1)])?;
        }
    }
    Ok(())
}
// pub async fn handle_wt_connection(connection: Connection) -> Result<()> {
//     let caster = screencast::Screencast::new().await?;
//     let mut config = get_config().await;
//     let session = caster.create_session().await?;
//     caster
//         .select_sources(
//             &session,
//             screencast::CursorMode::Embedded,
//             BitFlags::from_flag(config.capture_type.clone().into()),
//             false,
//             config.pipewire_key.as_deref(),
//             PersistMode::ExplicitlyRevoked,
//         )
//         .await?;
//     let pipewire_response = &caster
//         .start(&session, &WindowIdentifier::None)
//         .await?
//         .response()?;
//     config.pipewire_key = pipewire_response.restore_token().map(str::to_string);
//     update_config(config).await?;
//     // let streams = pipewire_response.streams();
//     let streams_fd = caster.open_pipe_wire_remote(&session).await?;
//     let mut file = unsafe { File::from_raw_fd(streams_fd.into_raw_fd()) };

//     let mut buf = [0 as u8; 2048];
//     loop {
//         if let Ok(_) = connection.receive_datagram().await {
//             for _ in 1..1000 {
//                 file.read_exact(&mut buf[..]).await?;
//                 eprintln!("Read screen data: {:x?}", &buf[..3]);
//                 connection.send_datagram(&buf)?;
//             }
//         }
//     }
// }
