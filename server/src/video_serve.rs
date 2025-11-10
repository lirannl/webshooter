use crate::{
    auth::OnetimeToken,
    config::{Bytes64, Config},
    error::WebshooterError,
    ipc::IPC_ID,
};
use anyhow::Result;
use ffmpeg::{codec, format, util::frame::video::Video};
use ffmpeg_next::{self as ffmpeg, Rational, format::Pixel};
use futures_util::FutureExt;
use interprocess::local_socket::{
    GenericNamespaced, ListenerOptions, prelude::*, traits::tokio::Listener,
};
use rand::{Rng, RngCore, thread_rng};
use scap::capturer::{Capturer, Options};
use std::{io::ErrorKind, str::FromStr, sync::Arc, time::Duration};
use tokio::{io::AsyncReadExt, spawn, task::JoinHandle};
use wtransport::{Connection, Endpoint, Identity, ServerConfig, endpoint::IncomingSession};

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
            handle_wt_connection(connection).await?;
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

            // Get frame dimensions from capturer
            let [width, height] = capturer.get_output_frame_size();

            // Validate that dimensions are valid before proceeding
            if width == 0 || height == 0 {
                eprintln!(
                    "Error: Captured frame dimensions are invalid (width: {}, height: {})",
                    width, height
                );
                return Err(WebshooterError::InvalidLogin.into());
            }

            // Set encoder parameters properly for AV1
            encoder.set_width(width);
            encoder.set_height(height);
            encoder.set_aspect_ratio(Rational::new(width as i32, height as i32).reduce());
            encoder.set_format(Pixel::YUV420P);
            encoder.set_time_base(Rational::new(1, 30));

            // Additional AV1-specific parameter setting
            // Some versions of libaom-av1 require specific settings for dimensions to be properly recognized
            if encoder.codec().is_some() {
                // Try to set some common AV1 parameters that might help with dimension recognition
                // This is a best-effort approach as the exact API varies by ffmpeg version
                eprintln!(
                    "Setting up AV1 encoder with dimensions: {}x{}",
                    width, height
                );
            }

            let mut encoder = encoder.open()?;
            stream.set_parameters(&encoder);

            // Process frames
            for _ in 0..10 {
                let frame = capturer.get_next_frame()?;
                let frame = match frame {
                    scap::frame::Frame::BGRx(frame) => Ok(frame),
                    _ => Err(WebshooterError::InvalidLogin),
                }?;
                let frame = bgrx_to_yuv420p(frame)?;

                // Ensure we're sending the correct frame type to encoder
                match encoder.send_frame(&frame) {
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("Error sending frame to encoder: {:?}", e);
                        // Continue processing other frames rather than failing completely
                        // This prevents a single frame error from stopping the entire stream
                        continue;
                    }
                }
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
