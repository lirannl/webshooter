use crate::{
    auth::OnetimeToken,
    config::{Bytes64, Config},
    error::WebshooterError,
    ipc::IPC_ID,
    logging::log,
};
use anyhow::{anyhow, Result};
use ffmpeg::{codec, codec::Context as CodecContext, encoder, format, util::frame::video::Video};
use ffmpeg_next::{self as ffmpeg, codec::traits::Encoder, format::Pixel, Rational};
use futures_util::{FutureExt, TryFutureExt};
use interprocess::local_socket::{
    prelude::*, traits::tokio::Listener, GenericNamespaced, ListenerOptions,
};
use scap::capturer::{Capturer, Options};
use std::{
    error::Error, future::Future, io::ErrorKind, pin::Pin, str::FromStr, sync::Arc, time::Duration,
};
use tokio::{
    io::{AsyncReadExt, BufReader},
    spawn,
    task::JoinHandle,
};
use wtransport::{endpoint::IncomingSession, Connection, Endpoint, Identity, ServerConfig};

pub async fn setup_wt(config: Config, identity: Identity) -> Result<()> {
    let server_config = ServerConfig::builder()
        .with_bind_default(config.http_config.port)
        .with_identity(&identity)
        .keep_alive_interval(Some(Duration::from_mins(5)))
        .build();

    let server = Endpoint::server(server_config)?;

    for _id in 0.. {
        let session = server.accept().await;
        tokio::spawn(async move {
            let connection = webtransport_auth(session).await?;
            handle_wt_connection(connection).await?;
            // .map_err(|err| anyhow!("Error during webtransport connection {id}: {err:#?}"))
            // .unwrap_or_else(log);
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
    let pipe_name = format!("{IPC_ID}-{}", connection.session_id());
    #[cfg(target_family = "unix")]
    let pipe_name = {
        let temp_dir = format!("/tmp/{IPC_ID}");
        std::fs::create_dir_all(&temp_dir).or_else(|err| match err {
            err if err.kind() == ErrorKind::AlreadyExists => Ok(()),
            err => Err(err),
        })?;
        format!("{temp_dir}/{pipe_name}.sock")
    };

    let video_fragment_proxy = proxy_video(connection.clone(), pipe_name.clone());
    async move {
        ffmpeg::init()?;
        let mut capturer = Capturer::build(Options {
            ..Default::default()
        })?;
        capturer.start_capture();

        if let Ok(_) = connection.receive_datagram().await {
            let mut output_ctx: ffmpeg::format::context::Output =
                format::output_as(&pipe_name, "webm")?;
            let codec = codec::Id::AV1;
            let mut stream = output_ctx.add_stream(codec)?;
            let mut encoder = codec::context::Context::new_with_codec(
                codec::encoder::find(codec).ok_or(ffmpeg::Error::InvalidData)?,
            )
            .encoder()
            .video()?;
            stream.set_parameters(&encoder);
            let [width, height] = capturer.get_output_frame_size();
            encoder.set_height(height);
            encoder.set_width(width);
            encoder.set_aspect_ratio(Rational::new(width as i32, height as i32).reduce());
            encoder.set_format(format::Pixel::YUV420P);
            encoder.set_time_base(Rational::new(1, 30));
            let mut encoder = encoder.open()?;
            stream.set_parameters(&encoder);
            // let mut buf = [0; 512];
            // let mut vec = Vec::new();
            for _ in 0..10 {
                let frame = capturer.get_next_frame()?;
                let frame = match frame {
                    scap::frame::Frame::BGRx(frame) => Ok(frame),
                    _ => Err(WebshooterError::InvalidLogin),
                }?;
                let frame = bgrx_to_yuv420p(frame)?;

                encoder.send_frame(&frame)?;
            }
        }
        Ok(())
    }
    .map(move |result| {
        video_fragment_proxy.abort();
        result
    })
    .await
}

fn bgrx_to_yuv420p(frame: scap::frame::BGRxFrame) -> Result<ffmpeg::frame::Video> {
    let width = frame.width.abs() as u32;
    let height = frame.height.abs() as u32;
    let mut src_frame = Video::empty();
    src_frame.set_format(Pixel::BGRZ);
    src_frame.set_width(width);
    src_frame.set_height(height);
    src_frame.data_mut(0).copy_from_slice(&frame.data);

    // Set up destination frame (YUV420P)
    let mut dst_frame = Video::empty();
    dst_frame.set_format(Pixel::YUV420P);
    dst_frame.set_width(width);
    dst_frame.set_height(height);

    // Create a scaling context
    let mut scaler = ffmpeg::software::scaling::Context::get(
        src_frame.format(),
        src_frame.width(),
        src_frame.height(),
        dst_frame.format(),
        dst_frame.width(),
        dst_frame.height(),
        ffmpeg::software::scaling::Flags::BILINEAR,
    )?;
    // Perform the conversion
    scaler.run(&src_frame, &mut dst_frame)?;
    Ok(dst_frame)
}

fn proxy_video(connection: Arc<Connection>, pipe_name: String) -> JoinHandle<Result<()>> {
    spawn(async move {
        let pipe_name = pipe_name.to_ns_name::<GenericNamespaced>()?;
        let pipe = ListenerOptions::new().name(pipe_name).create_tokio()?;
        while let Ok(mut encoded) = pipe.accept().await {
            while let mut buf = Vec::new()
                && encoded.read(&mut buf).await? > 0
            {
                connection.send_datagram(&buf)?;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
}
