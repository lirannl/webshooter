use anyhow::Result;

const HEADER: usize = 8;

#[derive(Debug, Clone)]
pub enum ServerDatagram {
    VideoFrame {
        frame_id: u16,
        frag_idx: u16,
        num_frags: u16,
        is_keyframe: bool,
        payload: Vec<u8>,
    },
}

impl ServerDatagram {
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::VideoFrame {
                frame_id,
                frag_idx,
                num_frags,
                is_keyframe,
                payload,
            } => {
                let mut buf = Vec::with_capacity(HEADER + payload.len());
                buf.push(0);
                buf.extend_from_slice(&frame_id.to_be_bytes());
                buf.extend_from_slice(&frag_idx.to_be_bytes());
                buf.extend_from_slice(&num_frags.to_be_bytes());
                buf.push(*is_keyframe as u8);
                buf.extend_from_slice(payload);
                buf
            }
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER {
            anyhow::bail!("Datagram too short: {} bytes", bytes.len());
        }
        match bytes[0] {
            0 => {
                let frame_id = u16::from_be_bytes([bytes[1], bytes[2]]);
                let frag_idx = u16::from_be_bytes([bytes[3], bytes[4]]);
                let num_frags = u16::from_be_bytes([bytes[5], bytes[6]]);
                let is_keyframe = (bytes[7] & 1) != 0;
                let payload = bytes[8..].to_vec();
                Ok(Self::VideoFrame {
                    frame_id,
                    frag_idx,
                    num_frags,
                    is_keyframe,
                    payload,
                })
            }
            d => anyhow::bail!("Invalid server datagram discriminant: {d}"),
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
