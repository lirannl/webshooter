use crate::codec::Codec;
use anyhow::Result;
use named_constants::named_constants;

/// 1 byte discriminant + 2×3 frame metadata + 1 is_keyframe + 1 codec = 9
const HEADER: usize = 9;

/// Maximum payload we send per audio datagram. Derived from the WebTransport
/// default `max_datagram_size` of 1200, minus the `AudioFrame` header (13).
pub const MAX_AUDIO_DATAGRAM_PAYLOAD: usize = 1187;

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
    VideoFrame {
        frame_id: u16,
        frag_idx: u16,
        num_frags: u16,
        is_keyframe: bool,
        codec: Codec,
        payload: Vec<u8>,
    },
    ReleaseMouse,
    ToggleFullscreen,
    AudioFrame {
        frame_id: u16,
        frag_idx: u16,
        num_frags: u16,
        channels: u8,
        rate: u32,
        format: AudioFormat,
        payload: Vec<u8>,
    },
}

impl ServerDatagram {
    /// Serialize a video frame directly from an already-encoded payload slice,
    /// avoiding an intermediate `Vec<u8>` allocation per fragment.
    pub fn video_frame_to_bytes(
        frame_id: u16,
        frag_idx: u16,
        num_frags: u16,
        is_keyframe: bool,
        codec: Codec,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER + payload.len());
        buf.push(ServerDatagramVariants::VIDEO_FRAME.0);
        buf.extend_from_slice(&frame_id.to_be_bytes());
        buf.extend_from_slice(&frag_idx.to_be_bytes());
        buf.extend_from_slice(&num_frags.to_be_bytes());
        buf.push(is_keyframe as u8);
        buf.push(codec.to_byte());
        buf.extend_from_slice(payload);
        buf
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::VideoFrame {
                frame_id,
                frag_idx,
                num_frags,
                is_keyframe,
                codec,
                payload,
            } => {
                let mut buf = Vec::with_capacity(HEADER + payload.len());
                buf.push(ServerDatagramVariants::VIDEO_FRAME.0);
                buf.extend_from_slice(&frame_id.to_be_bytes());
                buf.extend_from_slice(&frag_idx.to_be_bytes());
                buf.extend_from_slice(&num_frags.to_be_bytes());
                buf.push(*is_keyframe as u8);
                buf.push(codec.to_byte());
                buf.extend_from_slice(payload);
                buf
            }
            Self::ReleaseMouse => vec![ServerDatagramVariants::RELEASE_MOUSE.0],
            Self::ToggleFullscreen => vec![ServerDatagramVariants::TOGGLE_FULLSCREEN.0],
            Self::AudioFrame {
                frame_id,
                frag_idx,
                num_frags,
                channels,
                rate,
                format,
                payload,
            } => {
                let mut buf = Vec::with_capacity(13 + payload.len());
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
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            anyhow::bail!("Empty datagram");
        }
        match ServerDatagramVariants(bytes[0]) {
            ServerDatagramVariants::VIDEO_FRAME => {
                if bytes.len() < HEADER {
                    anyhow::bail!("VideoFrame datagram too short: {} bytes", bytes.len());
                }
                let frame_id = u16::from_be_bytes([bytes[1], bytes[2]]);
                let frag_idx = u16::from_be_bytes([bytes[3], bytes[4]]);
                let num_frags = u16::from_be_bytes([bytes[5], bytes[6]]);
                let is_keyframe = (bytes[7] & 1) != 0;
                let codec = Codec::from_byte(bytes[8])?;
                let payload = bytes[9..].to_vec();
                Ok(Self::VideoFrame {
                    frame_id,
                    frag_idx,
                    num_frags,
                    is_keyframe,
                    codec,
                    payload,
                })
            }
            ServerDatagramVariants::RELEASE_MOUSE => Ok(Self::ReleaseMouse),
            ServerDatagramVariants::TOGGLE_FULLSCREEN => Ok(Self::ToggleFullscreen),
            ServerDatagramVariants::AUDIO_FRAME => {
                // 1 discriminant + 2*3 frame metadata + 1 channels + 4 rate + 1 format
                const H: usize = 13;
                if bytes.len() < H {
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
            n => anyhow::bail!("Invalid server datagram discriminant: {}", n.0),
        }
    }

    pub const fn header_size() -> usize {
        HEADER
    }
}

/// Maximum safe payload per datagram given a max datagram size.
pub fn max_payload_size(max_datagram_size: usize) -> usize {
    max_datagram_size.saturating_sub(HEADER).max(1)
}
