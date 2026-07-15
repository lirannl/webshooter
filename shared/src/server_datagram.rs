use crate::codec::Codec;
use anyhow::Result;
use named_constants::named_constants;

/// 1 byte discriminant + 2×3 frame metadata + 1 is_keyframe + 1 codec = 9
const HEADER: usize = 9;

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
