//! Putting screen and audio into MoQ, as tracks on one broadcast.
//!
//! # What this replaces
//!
//! The screen used to go out as bespoke datagrams — one stream per keyframe and a
//! paced, fragmented, resend-repairable fan of datagrams per delta — and audio as
//! more of the same. None of that machinery survives. Here, both are frames on a
//! group, groups are cut by the one rule that matters for decodability (a keyframe
//! starts a group), and moq-net owns everything about putting them on the wire:
//! stream credit, flow control, congestion response, and dropping what has
//! fallen behind the live edge.
//!
//! # One group per GOP, and why that is the right cut
//!
//! A moq-lite group is carried on **one unidirectional stream** (see
//! `lite::publisher::GroupServe`). So the group boundary is the stream boundary,
//! and the cut has to balance two costs that pull opposite ways:
//!
//! - A group must *start* with a keyframe, or the frames after it are undecodable.
//!   One group per GOP is the coarsest cut that guarantees this, and it makes one
//!   stream per second or two instead of one per frame — the stream-credit churn
//!   that made the old per-frame scheme expensive simply does not happen.
//! - A subscriber that joins mid-group is served from wherever the group has got
//!   to, with `frame_start` naming the offset. That is safe only because the group
//!   began at a keyframe: the subscriber's first frames *are* decodable. Cutting
//!   groups more finely would not reduce that risk, and cutting them more coarsely
//!   would hold a stream (and its flow-control credit) open for longer.
//!
//! The ceiling on that coarseness is the encoder's own: one group per GOP,
//! however long the GOP runs. The client, not the server, decides when a
//! keyframe is needed (see the `RequestKeyframe` handling in `pipewire::video`),
//! and a group is never split mid-GOP — a group that does not start with a
//! keyframe is a group nobody can decode from.
//!
//! # Codec identity
//!
//! A track's name is its identity, so the codec is part of the video track's name:
//! `video/h264`. That puts the codec where a subscriber can read it off the stream
//! it subscribed to, and makes a codec change a *new track* rather than a
//! reinterpretation of an existing one — the old track is finished and the client
//! is told about the new one.
//!
//! Telling the client is the one bit of signalling that stays outside MoQ: track
//! names are not discoverable in moq-lite (a SUBSCRIBE names an exact track), so a
//! subscriber cannot learn which tracks exist without being told. That is
//! [`ServerDatagram::VideoTrack`], and it is a *control* message: it says which
//! track to subscribe to, and no media ever travels on it.

use std::time::Duration;

use anyhow::{Context as _, Result};
use moq_net::time::Instant;
use moq_net::{Server, Timescale, Timestamp, broadcast, group, kio, origin, track};
use shared::codec::Codec;
use shared::server_datagram::ServerDatagram;
use shared::track_names::{AUDIO_TRACK, BROADCAST, video_track};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::transport::WtSession;

/// There is no frame-count cap on a video group: a group *is* one GOP, so it
/// lasts exactly as long as the encoder needs for one, and the cut is always the
/// next keyframe. An earlier cap that split a GOP mid-way unilaterally introduced
/// exactly the bug MoQ's groups are there to avoid — a group that does *not*
/// start with a keyframe is a stream of frames a decoder cannot interpret no
/// matter how it joins. If an encoder ever goes unhealthily long between
/// keyframes, the right fix is to push the encoder, not to split the group.
/// Packets per audio group.
///
/// Audio frames have no inter-frame dependency, so the group boundary here is
/// purely a cost balance: each group is one stream, so too few means a stream every
/// few tens of milliseconds, and too many holds a stream open past the point where
/// the audio it carries could usefully have been delivered. At the negotiated rates
/// this is a couple of hundred milliseconds.
const AUDIO_GROUP_MAX_FRAMES: usize = 10;

/// How much history a subscriber may fall behind and still be served.
const MAX_AGE: Duration = Duration::from_secs(10);

/// One encoded video frame, ready to be appended to the video track.
pub(crate) struct VideoFrame<'a> {
    pub payload: &'a [u8],
    pub codec: Codec,
    pub is_keyframe: bool,
}

/// One encoded audio packet.
pub(crate) struct AudioFrame<'a> {
    pub payload: &'a [u8],
}

/// The video track and the group currently accepting frames.
struct Video {
    codec: Codec,
    track: track::Producer,
    /// `None` between groups. A group is open from the frame that opened it until
    /// the next keyframe (or the frame ceiling) closes it.
    group: Option<group::Producer>,
    frames: usize,
}

/// The content of one session: its broadcast, its tracks, and the rules for
/// cutting frames into groups.
///
/// Deliberately not the same type as [`Publisher`], which adds only a handshake
/// and the lifetime of the origin that handshake handed it. Nothing here touches
/// the transport, so the framing a subscriber depends on — a group that starts
/// with a keyframe, a frame whose first byte says whether it is one — is
/// exercisable against a local subscriber, which is where the tests in this module
/// drive it.
pub(crate) struct Tracks {
    broadcast: broadcast::Producer,
    video: Option<Video>,
    audio: track::Producer,
    audio_group: Option<group::Producer>,
    audio_frames: usize,
    /// Where the one-off "subscribe to this track" notice goes. Kept rather than
    /// borrowed so publishing never blocks on a control channel nobody is reading:
    /// a client that has gone away is a session that is about to end anyway.
    control_tx: mpsc::Sender<ServerDatagram>,
}

impl Tracks {
    /// Create and announce the broadcast, with its audio track already on it.
    ///
    /// Announced here, with only the audio track present. The video track is
    /// created on the first encoded frame — the codec is not known before then,
    /// because it is chosen from what the client said it can decode — and a track
    /// created after the announcement still resolves for a subscriber that asks
    /// for it afterwards. Which is the only order that works: the client subscribes
    /// to the video track only after being told its name, and it is told only once
    /// the track exists.
    pub(crate) fn new(
        origin: &origin::Producer,
        control_tx: mpsc::Sender<ServerDatagram>,
    ) -> Result<Self> {
        let broadcast = origin
            .create_broadcast(BROADCAST)
            .context("creating the MoQ broadcast")?;
        let audio = broadcast
            .create_track(AUDIO_TRACK, track_info())
            .context("creating the audio track")?;
        broadcast
            .announce(origin::Route::default())
            .context("announcing the MoQ broadcast")?;
        Ok(Self {
            broadcast,
            video: None,
            audio,
            audio_group: None,
            audio_frames: 0,
            control_tx,
        })
    }

    /// Append one encoded video frame, cutting a new group at a keyframe.
    ///
    /// Returns an error only for a failure that invalidates the track (it has been
    /// finished, or the origin has gone). Ordinary pressure — a frame too large for
    /// a group, a group too large for its track — is reported to the caller the same
    /// way, because in every one of those cases continuing to append to a track that
    /// has rejected a frame produces a stream the client cannot decode.
    pub(crate) fn push_video(&mut self, frame: VideoFrame<'_>) -> Result<()> {
        // A codec change is a new track, not a new codec on the old one: a track's
        // frames are all one codec, and the client builds its decoder from the
        // name it subscribed to.
        if self.video.as_ref().is_some_and(|v| v.codec != frame.codec) {
            self.close_video()?;
        }
        if self.video.is_none() {
            self.video = Some(self.open_video(frame.codec)?);
        }

        // One group per GOP: the encoder decides the cut with its keyframes,
        // and every group therefore starts with a decodable frame. Opening the
        // group here — not lazily at the next `write_frame` — is what
        // guarantees the frame that follows always has somewhere to go.
        let video = self.video.as_mut().expect("just opened");
        let cut = frame.is_keyframe;
        if cut {
            finish(video.group.take());
            video.group = Some(
                video
                    .track
                    .append_group()
                    .context("appending a video group")?,
            );
            video.frames = 0;
        }
        let group = video
            .group
            .as_mut()
            .expect("a group is open between the cut above and the next frame");
        // The key/delta classification is *inside* the payload: one tag byte
        // followed by the encoded frame. MoQ's frame type is definitionally silent —
        // the model carries every frame of a track the same way — and grouping
        // alone cannot carry it: a subscriber that joins mid-track lands in the
        // middle of a group, and the WebCodecs chunk *type* is worth reading from a
        // byte rather than re-derivable from stream position. One byte per frame
        // costs nothing against a delta at the multi-kilobyte scale.
        let mut chunk = Vec::with_capacity(frame.payload.len() + 1);
        chunk.push(u8::from(frame.is_keyframe));
        chunk.extend_from_slice(frame.payload);
        group
            .write_frame(Timestamp::now(), chunk)
            .context("writing a video frame")?;
        video.frames += 1;
        Ok(())
    }

    /// Append one encoded audio packet.
    pub(crate) fn push_audio(&mut self, frame: AudioFrame<'_>) -> Result<()> {
        if self.audio_group.is_none() || self.audio_frames >= AUDIO_GROUP_MAX_FRAMES {
            finish(self.audio_group.take());
            self.audio_group = Some(
                self.audio
                    .append_group()
                    .context("appending an audio group")?,
            );
            self.audio_frames = 0;
        }
        self.audio_group
            .as_mut()
            .expect("a group is open between the line above and the write")
            .write_frame(Timestamp::now(), frame.payload)
            .context("writing an audio frame")?;
        self.audio_frames += 1;
        Ok(())
    }

    /// Create the video track for `codec` and tell the client to subscribe to it.
    fn open_video(&self, codec: Codec) -> Result<Video> {
        let track = self
            .broadcast
            .create_track(video_track(codec), track_info())
            .context("creating the video track")?;
        // Best effort, and deliberately not awaited: this is the last thing standing
        // between the client and a subscription, so it goes out even if the control
        // channel is briefly full. A client that misses it re-reads its decoder
        // capabilities and the session restarts its media.
        if self
            .control_tx
            .try_send(ServerDatagram::VideoTrack { codec })
            .is_err()
        {
            log::warn!("could not tell the client which video track to subscribe to");
        }
        Ok(Video {
            codec,
            track,
            group: None,
            frames: 0,
        })
    }

    /// End the video track and its group, in that order.
    fn close_video(&mut self) -> Result<()> {
        if let Some(mut video) = self.video.take() {
            finish(video.group.take());
            if let Err(err) = video.track.finish() {
                log::warn!("finishing the video track: {err:#}");
            }
        }
        Ok(())
    }
}

/// The publishing side of one session's MoQ session: a handshake, and the
/// content that handshake opened.
///
/// Dropping this ends the session's content: the broadcast closes when its last
/// source goes, and a subscriber's tracks end rather than going quiet.
pub(crate) struct Publisher {
    /// Held so the origin outlives the tracks. An origin whose producer is dropped
    /// finishes, and everything below it with it — which is why this is stored and
    /// never read, rather than being dropped straight after the handshake.
    _origin: origin::Producer,
    tracks: Tracks,
    /// Taken by the media pump; aborted on drop if it was never taken.
    driver: Option<JoinHandle<()>>,
}

impl Publisher {
    /// Run the MoQ handshake on `session` and prepare to publish into it.
    ///
    /// The returned publisher owns the origin, tracks, and running driver. The
    /// caller should take the driver handle for session supervision. If the
    /// handshake fails the session is closed by `moq-net` before this returns,
    /// so there is nothing to clean up but the error.
    pub(crate) async fn start(
        session: WtSession,
        control_tx: mpsc::Sender<ServerDatagram>,
    ) -> Result<Self> {
        let (origin, origin_driver) = origin::Producer::new(origin::Config::default());
        let server = Server::new().with_publisher(&origin);
        let (moq, session_driver) = server
            .accept_lite(Instant::now(), session)
            .await
            .context("MoQ handshake")?;

        let tracks = Tracks::new(&origin, control_tx)?;

        // Both drivers are polled by one loop: the session driver moves bytes and
        // the origin driver moves content, and neither makes the other progress.
        let driver = tokio::spawn(async move {
            let outcome = moq_net::time::run(Both {
                session: session_driver,
                origin: origin_driver,
            })
            .await;
            log::debug!("MoQ session finished: {outcome}");
            // The protocol-level session handle holds the last clone of the
            // transport session; the origin handles this publisher no longer
            // needs. Dropping it here, rather than leaving it to the handshake's
            // caller, is what ends the MoQ session when the driver stops.
            drop(moq);
        });
        Ok(Self {
            _origin: origin,
            tracks,
            driver: Some(driver),
        })
    }

    /// Give the running MoQ driver to the session supervisor for tracking.
    /// If not taken, dropping the publisher aborts the driver instead of detaching it.
    pub(crate) fn take_driver(&mut self) -> Option<JoinHandle<()>> {
        self.driver.take()
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.take() {
            driver.abort();
        }
    }
}

/// The publisher *is* its content, so every frame goes through unchanged.
///
/// `Deref` rather than a hand-written forwarder per method: the two types differ
/// by a handshake and a lifetime, and duplicating the method list would be a way
/// for the two halves to drift apart.
impl std::ops::Deref for Publisher {
    type Target = Tracks;

    fn deref(&self) -> &Tracks {
        &self.tracks
    }
}

impl std::ops::DerefMut for Publisher {
    fn deref_mut(&mut self) -> &mut Tracks {
        &mut self.tracks
    }
}

/// Close a group, reporting rather than propagating the failure.
///
/// A group that is never finished is a stream that never closes: it holds its
/// stream credit and its flow-control window until the drift budget expires it,
/// which is a far worse outcome than finishing it a moment early. But the only
/// thing `finish` can report is that the track has already been finished — in
/// which case there is nothing left to publish to either, and the caller's next
/// `write_frame` will say so.
fn finish(group: Option<group::Producer>) {
    if let Some(group) = group
        && let Err(err) = group.finish()
    {
        log::warn!("finishing a group: {err:#}");
    }
}

impl Drop for Tracks {
    fn drop(&mut self) {
        // Both tracks and the group in progress are closed on the way out, so a
        // subscriber sees an end rather than a track that stopped without saying so.
        let _ = self.close_video();
        finish(self.audio_group.take());
        if let Err(err) = self.audio.finish() {
            log::warn!("finishing the audio track: {err:#}");
        }
    }
}

/// Both moq-net drivers on one poll loop.
///
/// `moq_net::time::run` drives exactly one driver, and this session has two: one
/// moves bytes over the transport, the other moves content between the origin and
/// the session. Neither wakes the other, so a loop that ran only one would poll the
/// other at an arbitrary rate — or, worse, treat its `Ok(None)` as "nothing to do".
struct Both {
    session: moq_net::Driver<WtSession>,
    origin: origin::Driver,
}

impl moq_net::time::Driver for Both {
    fn poll(
        &mut self,
        now: Instant,
        waiter: &kio::Waiter,
    ) -> Result<Option<Instant>, moq_net::Error> {
        // Both are polled every time round, including when one of them asks to be
        // woken later: the other may already have work, and returning its `None`
        // would drop that. The earliest deadline wins, since the loop sleeps once.
        //
        // The first error ends the session. That is the driver's own contract — an
        // error is terminal — and the content side has nothing to say about it: it
        // reads from the origin, which outlives this poll.
        let session_deadline = self.session.poll(now, waiter)?;
        let origin_deadline = self.origin.poll(now, waiter)?;
        Ok(match (session_deadline, origin_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(at), None) | (None, Some(at)) => Some(at),
            (None, None) => None,
        })
    }
}

/// Shared track parameters.
///
/// The timescale is left at its millisecond default because that is the unit
/// [`Timestamp::now`] produces: the capture pipelines hand out buffers with no
/// presentation timestamp to carry, so the model's own clock is the only honest
/// source, and it is read in milliseconds.
fn track_info() -> track::Info {
    track::Info::default()
        .with_timescale(Timescale::MILLI)
        .with_max_age(MAX_AGE)
}

/// What the publisher does, observed through the model.
///
/// These read the origin rather than a session: a local subscriber is served what
/// the model can still prove, which is enough to check *naming* — the broadcast's
/// name, a track's name, the notice that tells a client where to subscribe — and
/// that is where a disagreement between the two ends would show up. It is not
/// enough to check delivery: a frame read back through the same origin is served
/// differently than one that crossed a session, so an assertion about frame order
/// here would be about moq-net's local fast path rather than about this
/// publisher. Delivery is exercised by running the client.
#[cfg(test)]
mod tests {
    use super::*;
    use shared::codec::Codec;

    /// The origin every test publishes through, with its driver already running:
    /// an origin that is not polled does not move content.
    fn origin() -> origin::Producer {
        let (origin, driver) = origin::Producer::new(origin::Config::default());
        tokio::spawn(moq_net::time::run(driver));
        origin
    }

    /// `Tracks` with a control channel, so a test can see the notice that tells a
    /// client which track to subscribe to.
    fn tracks(origin: &origin::Producer) -> (Tracks, mpsc::Receiver<ServerDatagram>) {
        let (control_tx, control_rx) = mpsc::channel(8);
        (
            Tracks::new(origin, control_tx).expect("creating and announcing the broadcast"),
            control_rx,
        )
    }

    fn video(payload: &'static [u8], codec: Codec, is_keyframe: bool) -> VideoFrame<'static> {
        VideoFrame {
            payload,
            codec,
            is_keyframe,
        }
    }

    /// The announced broadcast, read through a consumer of the same origin — the
    /// closest a local test gets to the other side of the wire.
    async fn announced(origin: &origin::Producer) -> broadcast::Consumer {
        origin
            .consume()
            .request_broadcast(BROADCAST)
            .await
            .expect("the announced broadcast")
    }

    /// What a client is told to subscribe to, as a test sees it.
    fn notified(control: &mut mpsc::Receiver<ServerDatagram>) -> Option<Codec> {
        match control.try_recv() {
            Ok(ServerDatagram::VideoTrack { codec }) => Some(codec),
            Ok(other) => panic!("a video track notice was expected, got {other:?}"),
            Err(err) => None.or_else(|| panic!("no track notice was sent: {err}")),
        }
    }

    /// Whether a subscription to `name` can be established right now.
    ///
    /// Resolving a *name* never fails — the model hands back a consumer for any
    /// path — so this is the question that actually matters, and the one the
    /// client asks when it follows a `VideoTrack` notice. A name with no track
    /// under it is answered `NotFound` (or, for a track that has not been created
    /// yet, not answered at all), so a bounded wait covers both.
    async fn can_subscribe(broadcast: &broadcast::Consumer, name: &str) -> bool {
        let track = broadcast.track(name).expect("resolving a track name");
        tokio::time::timeout(
            Duration::from_millis(200),
            track.subscribe(track::Subscription::default()),
        )
        .await
        .is_ok_and(|subscribed| subscribed.is_ok())
    }

    /// What is on the wire is a name, and a name that disagrees is a subscriber
    /// that waits forever rather than an error. So the broadcast is announced
    /// under the one name both ends read from `shared::track_names`, and the audio
    /// track resolves inside it before any video exists.
    #[tokio::test]
    async fn the_broadcast_carries_the_audio_track_before_any_video() {
        let origin = origin();
        let (mut tracks, mut control) = tracks(&origin);
        let mut cursor = origin.consume().announced();
        let broadcast = announced(&origin).await;

        // The name on the wire is the shared one, not a literal repeated here.
        let update = cursor.next().await.expect("the broadcast to be announced");
        assert!(update.kind.is_active(), "the broadcast must be announced");
        assert_eq!(update.prefix.as_str(), BROADCAST);

        assert!(
            can_subscribe(&broadcast, AUDIO_TRACK).await,
            "the audio track must be subscribable as soon as the broadcast is announced"
        );
        // Nothing has been encoded, so there is no video track and nothing to tell
        // a client about.
        assert!(
            control.try_recv().is_err(),
            "a video track notice before any frame would name a track nobody can subscribe to"
        );
        tracks
            .push_video(video(b"key", Codec::H264, true))
            .expect("a keyframe");
        assert_eq!(notified(&mut control), Some(Codec::H264));
    }

    /// The codec is part of the track's name, and the notice carries it because
    /// nothing else advertises the name: a track is created on the first frame,
    /// and a client cannot subscribe to a name it has not been told.
    #[tokio::test]
    async fn the_first_frame_creates_the_track_its_codec_names() {
        let origin = origin();
        let (mut tracks, mut control) = tracks(&origin);
        let broadcast = announced(&origin).await;

        assert!(
            !can_subscribe(&broadcast, &video_track(Codec::Av1)).await,
            "no video track exists before the first frame"
        );
        tracks
            .push_video(video(b"key", Codec::Av1, true))
            .expect("the first keyframe");

        assert_eq!(notified(&mut control), Some(Codec::Av1));
        assert!(
            can_subscribe(&broadcast, &video_track(Codec::Av1)).await,
            "the track must be subscribable under the name the notice implies"
        );
        assert!(
            !can_subscribe(&broadcast, &video_track(Codec::Vp9)).await,
            "a codec whose track was never created must not resolve"
        );
    }

    /// A codec change is a new track under a new name, and the client is told
    /// again. Continuing on the old track would hand it frames encoded with a
    /// different codec — a stream its decoder cannot read, with nothing to say so.
    #[tokio::test]
    async fn a_codec_change_starts_a_second_track_and_notifies_again() {
        let origin = origin();
        let (mut tracks, mut control) = tracks(&origin);
        let broadcast = announced(&origin).await;

        tracks
            .push_video(video(b"h264", Codec::H264, true))
            .expect("an H.264 keyframe");
        assert_eq!(notified(&mut control), Some(Codec::H264));
        tracks
            .push_video(video(b"av1", Codec::Av1, true))
            .expect("an AV1 keyframe");
        assert_eq!(
            notified(&mut control),
            Some(Codec::Av1),
            "the client has to be told about the new name"
        );

        assert!(
            can_subscribe(&broadcast, &video_track(Codec::Av1)).await,
            "the new codec's track must exist under its own name"
        );
        assert!(
            !can_subscribe(&broadcast, &video_track(Codec::Vp9)).await,
            "a codec that was never used must not resolve"
        );

        // The notice is not repeated per frame: it is what tells a client where
        // to subscribe, and a client that has subscribed has no use for it again.
        tracks
            .push_video(video(b"av1-delta", Codec::Av1, false))
            .expect("a delta");
        assert!(
            control.try_recv().is_err(),
            "only the first frame of a track may notify"
        );
    }
}
