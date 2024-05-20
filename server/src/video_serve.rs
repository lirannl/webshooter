use std::{env, fs::File, io::BufReader, path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::sync::Notify;
use webrtc::{
    api::{
        media_engine::{MediaEngine, MIME_TYPE_H264},
        APIBuilder,
    },
    ice_transport::ice_server::RTCIceServer,
    media::{io::h264_reader::H264Reader, Sample},
    peer_connection::configuration::RTCConfiguration,
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::{track_local_static_sample::TrackLocalStaticSample, TrackLocal},
};

pub async fn setup_video() -> Result<()> {
    let mut engine = MediaEngine::default();

    engine.register_default_codecs()?;

    let api = APIBuilder::new().with_media_engine(engine).build();

    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.localhost:2424".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create a new RTCPeerConnection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    let notify_tx = Arc::new(Notify::new());
    let notify_video = notify_tx.clone();
    // let notify_audio = notify_tx.clone();

    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);
    let video_done_tx = done_tx.clone();
    // let audio_done_tx = done_tx.clone();

    #[cfg(debug_assertions)]
    let video_file = PathBuf::from_str(&env::var("HOME")?)?.join("webrtc_test.mp4");

    let video_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    // Add this newly created track to the PeerConnection
    let rtp_sender = peer_connection
        .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    // Read incoming RTCP packets
    // Before these packets are returned they are processed by interceptors. For things
    // like NACK this needs to be called.
    tokio::spawn(async move {
        let mut rtcp_buf = vec![0u8; 1500];
        while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
        Result::<()>::Ok(())
    });

    tokio::spawn(async move {
        // Open a H264 file and start reading using our H264Reader
        let file = File::open(&video_file)?;
        let reader = BufReader::new(file);
        let mut h264 = H264Reader::new(reader, 1_048_576);

        // Wait for connection established
        notify_video.notified().await;

        println!("play video from disk file {video_file:?}");

        // It is important to use a time.Ticker instead of time.Sleep because
        // * avoids accumulating skew, just calling time.Sleep didn't compensate for the time spent parsing the data
        // * works around latency issues with Sleep
        let mut ticker = tokio::time::interval(Duration::from_millis(33));
        loop {
            let nal = match h264.next_nal() {
                Ok(nal) => nal,
                Err(err) => {
                    println!("All video frames parsed and sent: {err}");
                    break;
                }
            };
            video_track
                .write_sample(&Sample {
                    data: nal.data.freeze(),
                    duration: Duration::from_secs(1),
                    ..Default::default()
                })
                .await?;

            let _ = ticker.tick().await;
        }
        let _ = video_done_tx.try_send(());
        Ok::<_, anyhow::Error>(())
    });
    tokio::select! {
        _ = done_rx.recv() => {
            println!("received done signal!");
        }
    };

    peer_connection.close().await?;

    Ok(())
}
