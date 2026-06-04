use crate::{
    auth::OnetimeToken,
    config::{Bytes64, CaptureType, Config},
    error::WebshooterError,
    get_config,
    logging::log,
    update_config,
};
use anyhow::{Result, anyhow};
use ashpd::{
    desktop::{
        CreateSessionOptions,
        screencast::{
            CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
            StartCastOptions,
        },
    },
    enumflags2::BitFlags,
};
use futures_util::{FutureExt, StreamExt};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as gst_app;
use std::{os::fd::IntoRawFd, str::FromStr, sync::Arc, time::Duration};
use wtransport::{Connection, Endpoint, Identity, ServerConfig, VarInt, endpoint::IncomingSession};

pub async fn setup_wt(config: Config, identity: Identity) -> Result<()> {
    let server_config = ServerConfig::builder()
        .with_bind_default(config.http_config.port)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_mins(5)))
        .build();

    let server = Endpoint::server(server_config)?;

    for _id in 0.. {
        let session = server.accept().await;
        tokio::spawn(async move {
            let connection = webtransport_auth(session).await?;
            handle_wt_connection(connection)
                .await
                .unwrap_or_else(|err| log(err));
            Ok::<_, anyhow::Error>(())
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
    let connection = Arc::new(connection);

    async move {
        if let Ok(_) = connection.receive_datagram().await {
            let _ = connection.send_datagram(b"Hello, World!");
            let screencast = Screencast::new().await?;
            let session = screencast
                .create_session(CreateSessionOptions::default())
                .await?;
            let virt_source_token = get_config()
                .await
                .capture_sources
                .into_iter()
                .find(|s| s.type_ == CaptureType::Virtual)
                .map(|s| s.session_token);
            let mut bitflags = BitFlags::empty();
            bitflags.insert(SourceType::Virtual);
            screencast
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(CursorMode::Embedded)
                        .set_restore_token(virt_source_token.as_deref())
                        .set_persist_mode(Some(ashpd::desktop::PersistMode::ExplicitlyRevoked))
                        .set_sources(bitflags),
                )
                .await?;
            let capture = screencast
                .start(&session, None, StartCastOptions::default())
                .await?
                .response()?;

            // Persist the new restore token so future sessions skip the picker.
            if let Some(token) = capture.restore_token() {
                let mut config = get_config().await;
                if let Some(source) = config
                    .capture_sources
                    .iter_mut()
                    .find(|s| s.type_ == CaptureType::Virtual)
                {
                    source.session_token = token.to_string();
                    update_config(config).await?;
                }
            }

            let stream = capture.streams().first().ok_or(anyhow!("no stream"))?;
            let fd = screencast
                .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
                .await?;
            let node_id = stream.pipe_wire_node_id();
            let raw_fd = fd.into_raw_fd();

            gst::init()?;

            let pipeline = gst::parse::launch(&format!(
                "pipewiresrc fd={raw_fd} path={node_id} \
                 ! vaapipostproc \
                 ! vaav1enc \
                 ! av1parse \
                 ! matroskamux streamable=true \
                 ! appsink name=sink sync=false"
            ))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;

            let appsink = pipeline
                .by_name("sink")
                .ok_or(anyhow!("no sink element"))?
                .downcast::<gst_app::AppSink>()
                .map_err(|_| anyhow!("not an appsink"))?;

            let conn = connection.clone();
            appsink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                        let _ = conn.send_datagram(&map);
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );

            // Keep the screencast proxy alive — dropping it tears down the portal session.
            let _screencast = screencast;

            pipeline.set_state(gst::State::Playing)?;

            let reason = tokio::select! {
                err = connection.closed() => format!("Wt closed {err:#?}"),
                _ = session.receive_closed() => "Pipewire closed".to_string(),
            };
            log(reason);

            pipeline.set_state(gst::State::Null)?;
            connection.close(VarInt::from_u32(0), b"Aborted");
            session.close().await?;
        }
        Ok(())
    }
    .await
}
