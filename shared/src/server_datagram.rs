use anyhow::Result;
use log::LevelFilter;
use named_constants::named_constants;

use crate::codec::Codec;

#[named_constants(preserve_original)]
#[repr(u8)]
#[derive(Debug, Clone, PartialEq, Eq, Reflection)]
pub enum ServerDatagram {
    /// The first discriminant is not zero on purpose. Every message on the wire
    /// leads with a byte, and that byte is also how a peer tells a MoQ message
    /// from one of ours on the shared WebTransport session — see
    /// [`crate::mux`]. MoQ owns everything at or below
    /// [`crate::mux::MOQ_LAST_BYTE`], so this protocol starts above it.
    ReleaseMouse = crate::mux::APP_FIRST_BYTE,
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
}

impl ServerDatagram {
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
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
        ];
        for (dgram, expected) in cases {
            assert_eq!(dgram.to_bytes(), expected, "bytes changed for {dgram:?}");
        }
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
        for byte in 0..=crate::mux::MOQ_LAST_BYTE {
            assert!(
                ServerDatagram::from_bytes(&[byte]).is_err(),
                "{byte:#04x} decoded as a control message, but it belongs to MoQ"
            );
        }
    }
}
