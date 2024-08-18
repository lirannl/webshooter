use std::time::Duration;

use crate::{auth::OnetimeToken, config::Config, error::WebshooterError, logging::log};
use anyhow::{anyhow, bail, Result};
use tokio::time::sleep;
use wtransport::{endpoint::SessionRequest, Endpoint, Identity, ServerConfig, VarInt};

pub async fn setup_wt(config: Config, identity: Identity) -> Result<()> {
    let server_config = ServerConfig::builder()
        .with_bind_default(config.http_config.port)
        .with_identity(&identity)
        .keep_alive_interval(Some(Duration::from_secs(10)))
        .build();

    let server = Endpoint::server(server_config)?;

    for id in 0.. {
        let session = server.accept().await;
        let config = config.clone();
        tokio::spawn(async move {
            match session.await {
                Ok(session) => handle_wt_connection(&config, session)
                    .await
                    .unwrap_or_else(|err| {
                        log(anyhow!(
                            "Error during webtransport connection {id}: {err:#?}"
                        ))
                    }),
                Err(err) => {
                    log(err);
                }
            }
        });
    }
    Ok(())
}

pub async fn handle_wt_connection(config: &Config, session: SessionRequest) -> Result<()> {
    let connection = session.accept().await?;
    let onetime = {
        let mut auth_receiver = connection.accept_uni().await?;
        let mut buf = [0; _];
        auth_receiver.read(&mut buf).await?;
        OnetimeToken::from(buf)
    };
    if !onetime.check().await {
        connection.close(
            VarInt::from_u32(1),
            b"Session not authorised. Onetime token invalid",
        );
        bail!(WebshooterError::NotAuthorized);
    }
    loop {
        tokio::select! {
            // stream = connection.accept_bi() => {
            //     let mut stream = stream?;
            //     info!("Accepted BI stream");

            //     let bytes_read = match stream.1.read(&mut buffer).await? {
            //         Some(bytes_read) => bytes_read,
            //         None => continue,
            //     };

            //     let str_data = std::str::from_utf8(&buffer[..bytes_read])?;

            //     info!("Received (bi) '{str_data}' from client");

            //     stream.0.write_all(b"ACK").await?;
            // }
            // stream = connection.accept_uni() => {
            //     let mut stream = stream?;
            //     info!("Accepted UNI stream");

            //     let bytes_read = match stream.read(&mut buffer).await? {
            //         Some(bytes_read) => bytes_read,
            //         None => continue,
            //     };

            //     let str_data = std::str::from_utf8(&buffer[..bytes_read])?;

            //     info!("Received (uni) '{str_data}' from client");

            //     let mut stream = connection.open_uni().await?.await?;
            //     stream.write_all(b"ACK").await?;
            // }
            dgram = connection.receive_datagram() => {
                let dgram = dgram?;
                let str_data = std::str::from_utf8(&dgram)?;

                eprintln!("Received (dgram) '{str_data}' from client");

                connection.send_datagram(b"ACK")?;
            }
        }
    }
    // for _ in 0..256 {
    //     sleep(Duration::from_secs(2)).await;

    //     let _ = connection.send_datagram(b"Hello world!");
    // }
    // connection.close(VarInt::from_u32(0), b"Datagram test complete");
    // let fut = join(capture(&config, &session), setup_input(&config, &session)).await;
    // fut.0?;
    // fut.1?;

    Ok(())
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
