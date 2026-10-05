//! The names the media side of a session is published under.
//!
//! MoQ addresses content by path, so the names below *are* the protocol between
//! the publisher and its subscribers: a name that disagrees is not a parse error
//! anywhere, it is a subscriber that waits forever for a track nobody named.
//! Both ends therefore read them from here rather than each writing the string
//! out.
//!
//! The audio track is fixed, and the video track is not: its name carries the
//! codec, because a track's frames are all one codec and a subscriber builds its
//! decoder from the name it subscribed to. Nothing advertises the video track's
//! name, so the publisher names it in a control message once it has an encoder
//! (see `ServerDatagram::VideoTrack`).

use crate::codec::Codec;

/// The single broadcast every session's tracks live in.
///
/// One broadcast rather than two because there is one publisher and one
/// consumer; splitting audio and video across broadcasts would buy nothing and
/// cost a second announce to track.
pub const BROADCAST: &str = "webshooter";

/// The audio track's name inside [`BROADCAST`].
///
/// Fixed because the encoder and decoder are agreed out of band: the client
/// negotiates Opus in `AudioReady` and configures its decoder from its own audio
/// context, so neither end needs to be told.
pub const AUDIO_TRACK: &str = "audio/opus";

/// The video track's name for `codec`, inside [`BROADCAST`].
///
/// A change of codec is a new track, not a new name on the old one: the frames
/// already queued under the previous name were encoded differently, and a
/// subscriber that treated them as one stream would decode them with the wrong
/// decoder.
pub fn video_track(codec: Codec) -> String {
    format!("video/{}", codec.slug())
}
