use crate::codec::Codec;
use log::LevelFilter;
use anyhow::Result;
use named_constants::named_constants;

/// 1 discriminant + 1×u16 frame id + 1 codec. A keyframe is never fragmented,
/// so it carries no fragment fields at all.
const KEY_FRAME_HEADER: usize = 1 + size_of::<u16>() + 1;

/// 1 discriminant + 3×u16 frame metadata (id, fragment index, fragment count)
/// + 1 codec. The largest video header, so it is what bounds a datagram payload.
const DELTA_HEADER: usize = 1 + 3 * size_of::<u16>() + 1;

/// 1 discriminant + 3×u16 frame metadata + 1 channels + 1 rate (u32) + 1 format.
const AUDIO_HEADER: usize = 1 + 3 * size_of::<u16>() + 1 + size_of::<u32>() + 1;

/// Maximum payload we send per audio datagram. Derived from the WebTransport
/// default max datagram size of 1200, minus the [`AUDIO_HEADER`] header.
pub const MAX_AUDIO_DATAGRAM_PAYLOAD: usize = 1200 - AUDIO_HEADER;

/// Format of the bytes carried by [`ServerDatagram::AudioFrame`]. Audio is
/// always sent as a single encoded Opus packet (RFC 6716).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    /// One Opus packet (RFC 6716), always 48 kHz.
    Opus = 0,
}

impl AudioFormat {
    pub fn from_byte(b: u8) -> Result<Self> {
        Ok(match b {
            0 => Self::Opus,
            d => anyhow::bail!("Invalid audio format discriminant: {d}"),
        })
    }

    pub fn to_byte(self) -> u8 {
        self as u8
    }
}

#[named_constants(preserve_original)]
#[repr(u8)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerDatagram {
    /// A complete keyframe, on its own unidirectional stream.
    ///
    /// Split from [`Self::VideoDelta`] rather than sharing one variant with an
    /// `is_keyframe` flag so that being a keyframe is a property of the type
    /// instead of a field someone has to remember to set. A keyframe is the one
    /// frame the client cannot decode without, so it is the one frame that must
    /// never be dropped — and it must never be split, because a fragment lost in
    /// transit would leave the client with no reference at all. Having no
    /// `frag_idx`/`num_frags` here makes that unrepresentable rather than a
    /// convention.
    VideoKeyFrame {
        frame_id: u16,
        codec: Codec,
        payload: Vec<u8>,
    },
    ReleaseMouse,
    ToggleFullscreen,
    /// Announces the server's configured maximum log level so the client
    /// stops generating (and forwarding) records below that severity.
    LogLevel {
        level: LevelFilter,
    },
    AudioFrame {
        frame_id: u16,
        frag_idx: u16,
        num_frags: u16,
        channels: u8,
        rate: u32,
        format: AudioFormat,
        payload: Vec<u8>,
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
    /// A delta frame, split across as many datagrams as it needs.
    ///
    /// Declared last on purpose: the discriminant is its position, so a variant
    /// inserted above this one would silently renumber every message after it
    /// and change the wire format of things this has nothing to do with.
    VideoDelta {
        frame_id: u16,
        frag_idx: u16,
        num_frags: u16,
        codec: Codec,
        payload: Vec<u8>,
    },
}

impl ServerDatagram {
    /// Serialize a whole keyframe from an already-encoded payload slice,
    /// avoiding an intermediate `Vec<u8>` allocation.
    pub fn key_frame_to_bytes(frame_id: u16, codec: Codec, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(KEY_FRAME_HEADER + payload.len());
        buf.push(ServerDatagramVariants::VIDEO_KEY_FRAME.0);
        buf.extend_from_slice(&frame_id.to_be_bytes());
        buf.push(codec.to_byte());
        buf.extend_from_slice(payload);
        buf
    }

    /// Serialize one fragment of a delta directly from an already-encoded
    /// payload slice, avoiding an intermediate `Vec<u8>` allocation per
    /// fragment — a wide delta is a dozen of these.
    pub fn delta_to_bytes(
        frame_id: u16,
        frag_idx: u16,
        num_frags: u16,
        codec: Codec,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(DELTA_HEADER + payload.len());
        buf.push(ServerDatagramVariants::VIDEO_DELTA.0);
        buf.extend_from_slice(&frame_id.to_be_bytes());
        buf.extend_from_slice(&frag_idx.to_be_bytes());
        buf.extend_from_slice(&num_frags.to_be_bytes());
        buf.push(codec.to_byte());
        buf.extend_from_slice(payload);
        buf
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::VideoKeyFrame {
                frame_id,
                codec,
                payload,
            } => Self::key_frame_to_bytes(*frame_id, *codec, payload),
            Self::VideoDelta {
                frame_id,
                frag_idx,
                num_frags,
                codec,
                payload,
            } => Self::delta_to_bytes(*frame_id, *frag_idx, *num_frags, *codec, payload),
            Self::ReleaseMouse => vec![ServerDatagramVariants::RELEASE_MOUSE.0],
            Self::ToggleFullscreen => vec![ServerDatagramVariants::TOGGLE_FULLSCREEN.0],
            Self::LogLevel { level } => vec![
                ServerDatagramVariants::LOG_LEVEL.0,
                crate::log_level::filter_to_byte(*level),
            ],
            Self::AudioFrame {
                frame_id,
                frag_idx,
                num_frags,
                channels,
                rate,
                format,
                payload,
            } => {
                let mut buf = Vec::with_capacity(AUDIO_HEADER + payload.len());
                buf.push(ServerDatagramVariants::AUDIO_FRAME.0);
                buf.extend_from_slice(&frame_id.to_be_bytes());
                buf.extend_from_slice(&frag_idx.to_be_bytes());
                buf.extend_from_slice(&num_frags.to_be_bytes());
                buf.push(*channels);
                buf.extend_from_slice(&rate.to_be_bytes());
                buf.push(format.to_byte());
                buf.extend_from_slice(payload);
                buf
            }
            Self::Throttle { interval_ms } => {
                let mut buf = Vec::with_capacity(3);
                buf.push(ServerDatagramVariants::THROTTLE.0);
                buf.extend_from_slice(&interval_ms.to_be_bytes());
                buf
            }
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            anyhow::bail!("Empty datagram");
        }
        match ServerDatagramVariants(bytes[0]) {
            ServerDatagramVariants::VIDEO_KEY_FRAME => {
                if bytes.len() < KEY_FRAME_HEADER {
                    anyhow::bail!("VideoKeyFrame datagram too short: {} bytes", bytes.len());
                }
                let frame_id = u16::from_be_bytes([bytes[1], bytes[2]]);
                let codec = Codec::from_byte(bytes[3])?;
                let payload = bytes[4..].to_vec();
                Ok(Self::VideoKeyFrame {
                    frame_id,
                    codec,
                    payload,
                })
            }
            ServerDatagramVariants::VIDEO_DELTA => {
                if bytes.len() < DELTA_HEADER {
                    anyhow::bail!("VideoDelta datagram too short: {} bytes", bytes.len());
                }
                let frame_id = u16::from_be_bytes([bytes[1], bytes[2]]);
                let frag_idx = u16::from_be_bytes([bytes[3], bytes[4]]);
                let num_frags = u16::from_be_bytes([bytes[5], bytes[6]]);
                let codec = Codec::from_byte(bytes[7])?;
                let payload = bytes[8..].to_vec();
                Ok(Self::VideoDelta {
                    frame_id,
                    frag_idx,
                    num_frags,
                    codec,
                    payload,
                })
            }
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
            ServerDatagramVariants::AUDIO_FRAME => {
                if bytes.len() < AUDIO_HEADER {
                    anyhow::bail!("AudioFrame datagram too short: {} bytes", bytes.len());
                }
                let frame_id = u16::from_be_bytes([bytes[1], bytes[2]]);
                let frag_idx = u16::from_be_bytes([bytes[3], bytes[4]]);
                let num_frags = u16::from_be_bytes([bytes[5], bytes[6]]);
                let channels = bytes[7];
                let rate = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
                let format = AudioFormat::from_byte(bytes[12])?;
                let payload = bytes[13..].to_vec();
                Ok(Self::AudioFrame {
                    frame_id,
                    frag_idx,
                    num_frags,
                    channels,
                    rate,
                    format,
                    payload,
                })
            }
            ServerDatagramVariants::THROTTLE => {
                if bytes.len() < 3 {
                    anyhow::bail!("Throttle datagram too short: {} bytes", bytes.len());
                }
                let interval_ms = u16::from_be_bytes([bytes[1], bytes[2]]);
                Ok(Self::Throttle { interval_ms })
            }
            n => anyhow::bail!("Invalid server datagram discriminant: {}", n.0),
        }
    }

    /// The largest video header, which is what bounds the payload that fits one
    /// datagram. Deltas carry the fragment fields keyframes do not, so a payload
    /// sized for a keyframe would overflow a delta.
    pub const fn video_header_size() -> usize {
        DELTA_HEADER
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the exact wire bytes for every server variant (see the analogous
    /// test in `client_datagram.rs` for why this must never drift).
    #[test]
    fn wire_bytes_are_stable() {
        let cases: Vec<(ServerDatagram, Vec<u8>)> = vec![
            (
                // A keyframe is whole by construction: no fragment fields on the
                // wire at all, so it cannot be split by accident.
                ServerDatagram::VideoKeyFrame {
                    frame_id: 0x1234,
                    codec: Codec::Av1,
                    payload: vec![0xAA, 0xBB],
                },
                vec![0x00, 0x12, 0x34, 0x00, 0xAA, 0xBB],
            ),
            (
                ServerDatagram::VideoDelta {
                    frame_id: 0x1234,
                    frag_idx: 0x0056,
                    num_frags: 0x0003,
                    codec: Codec::Av1,
                    payload: vec![0xAA, 0xBB],
                },
                vec![0x06, 0x12, 0x34, 0x00, 0x56, 0x00, 0x03, 0x00, 0xAA, 0xBB],
            ),
            (ServerDatagram::ReleaseMouse, vec![0x01]),
            (ServerDatagram::ToggleFullscreen, vec![0x02]),
            (
                ServerDatagram::LogLevel {
                    level: log::LevelFilter::Info,
                },
                vec![0x03, 0x03],
            ),
            (
                ServerDatagram::AudioFrame {
                    frame_id: 0x0001,
                    frag_idx: 0,
                    num_frags: 1,
                    channels: 2,
                    rate: 48000,
                    format: AudioFormat::Opus,
                    payload: vec![0x11],
                },
                vec![0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00, 0xBB, 0x80, 0x00, 0x11],
            ),
            (
                ServerDatagram::Throttle { interval_ms: 300 },
                vec![0x05, 0x01, 0x2C],
            ),
        ];
        for (dgram, expected) in cases {
            assert_eq!(dgram.to_bytes(), expected, "bytes changed for {dgram:?}");
        }
    }
}
