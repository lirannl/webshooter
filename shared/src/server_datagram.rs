use anyhow::Result;
use log::{Level, LevelFilter};
use named_constants::named_constants;

use crate::codec::Codec;

#[named_constants(preserve_original)]
#[repr(u8)]
#[derive(Debug, Clone, PartialEq, Eq, Reflection)]
pub enum ServerDatagram {
    /// The boundary between MoQ and this protocol, and the only place that
    /// number is written down.
    ///
    /// **Never constructed and never sent.** It exists so the boundary has a
    /// name in [`ServerDatagramVariants`], which is what the rest of the crate
    /// then refers to — the router in [`crate::mux`], and the first discriminant
    /// of [`ClientDatagram`](crate::client_datagram::ClientDatagram), which
    /// shares this session and therefore must start at the same byte.
    ///
    /// It is a variant rather than a free constant because it is a fact about
    /// the wire format, and the enum *is* the wire format. A constant beside it
    /// would be a second statement of the same fact, free to drift.
    ///
    /// Carried here and not on a real message, so that reordering, removing or
    /// repurposing any message can never move the boundary by accident.
    ///
    /// The literal is `0x40` because MoQ owns the one-byte range at or below
    /// `0x3F`: every message on the wire leads with a byte, and that byte is also
    /// how a peer tells a MoQ message from one of ours. `0x3F` and not `0x01`
    /// because a MoQ *datagram* leads with a subscribe id rather than a type tag,
    /// and those ids grow; [`crate::mux`] works through why the whole byte is
    /// reserved.
    MoqBoundary = 0x40,
    ReleaseMouse,
    ToggleFullscreen,
    /// Announces the server's configured maximum log level so the client
    /// stops generating (and forwarding) records below that severity.
    LogLevel {
        level: LevelFilter,
    },
    /// Asks the client to reduce its input event rate. `interval_ms` is the
    /// minimum spacing the client must keep between consecutive input
    /// datagrams (0 = no throttling). Sent by the server when its input
    /// processing pipeline approaches saturation so it can shed load before
    /// events start blocking or overflowing. The client keeps sending the
    /// newest input state, just not more often than this interval.
    Throttle {
        interval_ms: u16,
    },
    /// Host system output volume setting as a fraction of maximum (0-255).
    /// Sent on a reliable stream at session start and whenever the host volume
    /// setting changes. The client may use this to synchronise its device
    /// volume to match the host's output volume.
    AudioLevel {
        level: u8,
    },
    /// Names the MoQ track carrying encoded video, so the client knows what to
    /// subscribe to.
    ///
    /// The client is told, once per track, and
    /// the video itself arrives over MoQ like everything else.
    ///
    /// Sent again if the codec changes: the old track is finished and a new one
    /// created under a new name, because a track's frames are all one codec and the
    /// client builds its decoder from the name it subscribed to.
    VideoTrack {
        codec: Codec,
    },
    /// Tells the client something the server cannot do for it right now, in a
    /// form a person can be shown.
    ///
    /// The mirror of [`crate::client_datagram::ClientDatagram::Error`]: the
    /// client already reports its own failures this way, and a server-side
    /// refusal — a locked session, no capture permission — is just as useless to
    /// the client unless it arrives as a message rather than as a session that
    /// silently never starts. A report, not a directive: nothing here is acted
    /// on by the client on the server's behalf.
    ///
    /// Appended last so the discriminants above keep their wire values; see
    /// `tests::wire_bytes_are_stable`.
    Error {
        level: Level,
        message: String,
    },
}

impl ServerDatagram {
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            // Never sent. The variant exists only to give the boundary byte a
            // name, so there is nothing to encode. The match has to be
            // exhaustive, which means this arm is the only thing standing
            // between "names a boundary" and "is a message" — it fails loudly
            // rather than quietly putting a meaningless byte on the wire.
            Self::MoqBoundary => {
                panic!("MoqBoundary names the wire boundary, it is not a message")
            }
            Self::ReleaseMouse => vec![ServerDatagramVariants::RELEASE_MOUSE.0],
            Self::ToggleFullscreen => vec![ServerDatagramVariants::TOGGLE_FULLSCREEN.0],
            Self::LogLevel { level } => vec![
                ServerDatagramVariants::LOG_LEVEL.0,
                crate::log_level::filter_to_byte(*level),
            ],
            Self::Throttle { interval_ms } => {
                let mut buf = Vec::with_capacity(3);
                buf.push(ServerDatagramVariants::THROTTLE.0);
                buf.extend_from_slice(&interval_ms.to_be_bytes());
                buf
            }
            Self::AudioLevel { level } => {
                vec![ServerDatagramVariants::AUDIO_LEVEL.0, *level]
            }
            Self::VideoTrack { codec } => {
                vec![ServerDatagramVariants::VIDEO_TRACK.0, codec.to_byte()]
            }
            Self::Error { level, message } => {
                let mut buf = Vec::with_capacity(2 + message.len());
                buf.push(ServerDatagramVariants::ERROR.0);
                buf.push(crate::log_level::level_to_byte(*level));
                buf.extend_from_slice(message.as_bytes());
                buf
            }
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            anyhow::bail!("Empty datagram");
        }
        match ServerDatagramVariants(bytes[0]) {
            ServerDatagramVariants::RELEASE_MOUSE => Ok(Self::ReleaseMouse),
            ServerDatagramVariants::TOGGLE_FULLSCREEN => Ok(Self::ToggleFullscreen),
            ServerDatagramVariants::LOG_LEVEL => {
                if bytes.len() < 2 {
                    anyhow::bail!("LogLevel datagram too short: {} bytes", bytes.len());
                }
                Ok(Self::LogLevel {
                    level: crate::log_level::filter_from_byte(bytes[1])?,
                })
            }
            ServerDatagramVariants::THROTTLE => {
                if bytes.len() < 3 {
                    anyhow::bail!("Throttle datagram too short: {} bytes", bytes.len());
                }
                let interval_ms = u16::from_be_bytes([bytes[1], bytes[2]]);
                Ok(Self::Throttle { interval_ms })
            }
            ServerDatagramVariants::AUDIO_LEVEL => {
                if bytes.len() < 2 {
                    anyhow::bail!("AudioLevel datagram too short: {} bytes", bytes.len());
                }
                Ok(Self::AudioLevel { level: bytes[1] })
            }
            ServerDatagramVariants::VIDEO_TRACK => {
                if bytes.len() < 2 {
                    anyhow::bail!("VideoTrack datagram too short: {} bytes", bytes.len());
                }
                Ok(Self::VideoTrack {
                    codec: Codec::from_byte(bytes[1])?,
                })
            }
            ServerDatagramVariants::ERROR => {
                let Some((&level, message)) = bytes[1..].split_first() else {
                    anyhow::bail!("Error datagram too short: {} bytes", bytes.len());
                };
                Ok(Self::Error {
                    level: crate::log_level::level_from_byte(level)?,
                    message: String::from_utf8_lossy(message).into_owned(),
                })
            }
            n => anyhow::bail!("Invalid server datagram discriminant: {}", n.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the exact wire bytes for every server variant. The media variants are
    /// gone — video and audio go out as MoQ frames now — so what remains is the
    /// control protocol, and this is the only thing that keeps its discriminants
    /// from being renumbered by an innocent reordering.

    #[test]
    fn wire_bytes_are_stable() {
        let cases: Vec<(ServerDatagram, Vec<u8>)> = vec![
            (
                ServerDatagram::ReleaseMouse,
                vec![ServerDatagramVariants::RELEASE_MOUSE.0],
            ),
            (
                ServerDatagram::ToggleFullscreen,
                vec![ServerDatagramVariants::TOGGLE_FULLSCREEN.0],
            ),
            (
                ServerDatagram::LogLevel {
                    level: log::LevelFilter::Info,
                },
                vec![
                    ServerDatagramVariants::LOG_LEVEL.0,
                    crate::log_level::filter_to_byte(log::LevelFilter::Info),
                ],
            ),
            (
                ServerDatagram::Throttle { interval_ms: 300 },
                vec![ServerDatagramVariants::THROTTLE.0, 0x01, 0x2C],
            ),
            (
                ServerDatagram::AudioLevel { level: 128 },
                vec![ServerDatagramVariants::AUDIO_LEVEL.0, 0x80],
            ),
            (
                ServerDatagram::VideoTrack { codec: Codec::H264 },
                vec![ServerDatagramVariants::VIDEO_TRACK.0, 0x02],
            ),
            (
                ServerDatagram::Error {
                    level: log::Level::Warn,
                    message: "session is locked".into(),
                },
                vec![
                    ServerDatagramVariants::ERROR.0,
                    crate::log_level::level_to_byte(log::Level::Warn),
                    b's',
                    b'e',
                    b's',
                    b's',
                    b'i',
                    b'o',
                    b'n',
                    b' ',
                    b'i',
                    b's',
                    b' ',
                    b'l',
                    b'o',
                    b'c',
                    b'k',
                    b'e',
                    b'd',
                ],
            ),
        ];
        for (dgram, expected) in cases {
            assert_eq!(dgram.to_bytes(), expected, "bytes changed for {dgram:?}");
        }
    }

    /// The boundary variant names a byte; it is not a message. Nothing may send it
    /// and nothing may parse it back, or the boundary would become a wire value
    /// that peers could use for two meanings at once.
    #[test]
    fn the_boundary_is_not_a_message() {
        assert!(
            ServerDatagram::from_bytes(&[ServerDatagramVariants::MOQ_BOUNDARY.0]).is_err(),
            "the boundary byte must not decode as a message"
        );
        assert!(
            !crate::mux::is_moq_byte(ServerDatagramVariants::MOQ_BOUNDARY.0),
            "the boundary byte must not fall inside MoQ's range"
        );
    }

    /// Every variant has to survive the round trip, not just the bytes: a
    /// variant that parses into the wrong message is worse than one that fails
    /// to parse at all.
    #[test]
    fn every_variant_round_trips() {
        let cases = [
            ServerDatagram::ReleaseMouse,
            ServerDatagram::ToggleFullscreen,
            ServerDatagram::LogLevel {
                level: log::LevelFilter::Debug,
            },
            ServerDatagram::Throttle { interval_ms: 1 },
            ServerDatagram::AudioLevel { level: 255 },
            ServerDatagram::VideoTrack { codec: Codec::Av1 },
            ServerDatagram::VideoTrack { codec: Codec::Vp9 },
            ServerDatagram::Error {
                level: log::Level::Warn,
                message: "the session is locked".into(),
            },
            // An empty message is legal: it must not be mistaken for a truncated
            // datagram, and it must not be the thing that decides that.
            ServerDatagram::Error {
                level: log::Level::Error,
                message: String::new(),
            },
        ];
        for dgram in cases {
            let bytes = dgram.to_bytes();
            assert_eq!(
                ServerDatagram::from_bytes(&bytes).expect("parses"),
                dgram,
                "round trip changed {dgram:?}"
            );
        }
    }

    /// Every discriminant in this family has to land above MoQ's reserved
    /// range, because the first byte is all a receiver has to route on. Walking
    /// the family rather than checking the first variant means a variant
    /// appended later cannot slip back down under the boundary.
    #[test]
    fn every_discriminant_lands_in_the_app_range() {
        for variant in ServerDatagramVariants::_values() {
            assert!(
                !crate::mux::is_moq_byte(variant.0),
                "{variant:?} would be routed to MoQ"
            );
        }
    }

    /// The same collision as [`ClientDatagram`](crate::client_datagram)'s
    /// equivalent test, from the receiving end: a message the mux routed to
    /// MoQ must not then parse as a control message. `ReleaseMouse` used to be
    /// `0x00`, which is exactly what a MoQ group stream starts with.
    #[test]
    fn a_moq_first_byte_is_not_a_control_discriminant() {
        for byte in 0..ServerDatagramVariants::RELEASE_MOUSE.0 {
            assert!(
                ServerDatagram::from_bytes(&[byte]).is_err(),
                "{byte:#04x} decoded as a control message, but it belongs to MoQ"
            );
        }
    }

    /// `Error` is the first variable-length server datagram — every other one is
    /// one or two fixed bytes — so it is the first whose truncation could read
    /// past the end of a buffer. It sits on the network receive path, where a
    /// panic takes the session down with it, so every cut short of a complete
    /// message must be an `Err` rather than a read out of bounds.
    #[test]
    fn a_truncated_error_never_panics() {
        let full = ServerDatagram::Error {
            level: log::Level::Warn,
            message: "the session is locked".into(),
        }
        .to_bytes();
        // Discriminant and level are the fixed part; a cut below that cannot be
        // told apart from noise and must be rejected.
        for cut in 0..2 {
            assert!(
                ServerDatagram::from_bytes(&full[..cut]).is_err(),
                "a {cut}-byte Error must not parse"
            );
        }
        // A level with the message cut off is a *legal* Error with nothing to
        // say, not a parse failure — an empty message round-trips above, so
        // truncating into one must be consistent with that rather than an error.
        assert_eq!(
            ServerDatagram::from_bytes(&full[..2]).expect("level alone parses"),
            ServerDatagram::Error {
                level: log::Level::Warn,
                message: String::new(),
            }
        );
        // Every other cut is a partial message: parsed as an empty one, never a
        // panic.
        for cut in 3..full.len() {
            let _ = ServerDatagram::from_bytes(&full[..cut]);
        }
    }

    /// The level is what makes an `Error` worth reading, so an unrecognised one
    /// is rejected rather than defaulted — a message with a wrong severity is
    /// worse than a dropped one.
    #[test]
    fn an_error_with_an_unknown_level_is_rejected() {
        let bytes = [
            ServerDatagramVariants::ERROR.0,
            0xFE, // not a level
            b'h',
            b'i',
        ];
        assert!(ServerDatagram::from_bytes(&bytes).is_err());
    }
}
