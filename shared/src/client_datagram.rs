
use anyhow::Result;

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Modifiers: u8 {
        const SHIFT = 1;
        const CTRL  = 2;
        const ALT   = 4;
        const META  = 8;
    }
}

#[repr(u8)]
#[derive(Debug, Clone)]
pub enum ClientDatagram {
    KeepAlive,
    Keyboard {
        keycode: String,
        modifiers: Modifiers,
    },
    ResizeDisplay {
        index: u8,
        width: u16,
        height: u16,
    },
    TouchscreenMulti {
        touches: Vec<(u8, u16, u16)>,
    },
    TouchscreenRelease {
        index: u8,
    },
    Error {
        message: String,
    },
}

impl ClientDatagram {
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::KeepAlive => vec![0],
            Self::Keyboard { keycode, modifiers } => {
                let key_bytes = keycode.as_bytes();
                let mut buf = Vec::with_capacity(2 + key_bytes.len());
                buf.push(1);
                buf.push(modifiers.bits());
                buf.extend_from_slice(key_bytes);
                buf
            }
            Self::ResizeDisplay {
                index,
                width,
                height,
            } => {
                let mut buf = Vec::with_capacity(6);
                buf.push(2);
                buf.push(*index);
                buf.extend_from_slice(&width.to_be_bytes());
                buf.extend_from_slice(&height.to_be_bytes());
                buf
            }
            Self::TouchscreenMulti { touches } => {
                let mut buf = Vec::with_capacity(1 + 1 + touches.len() * 5);
                buf.push(3);
                buf.push(touches.len() as u8);
                for (index, x, y) in touches {
                    buf.push(*index);
                    buf.extend_from_slice(&x.to_be_bytes());
                    buf.extend_from_slice(&y.to_be_bytes());
                }
                buf
            }
            Self::TouchscreenRelease { index } => {
                vec![4, *index]
            }
            Self::Error { message } => {
                let mut buf = Vec::with_capacity(message.len() + 1);
                buf.push(5);
                buf.extend_from_slice(message.as_bytes());
                buf
            }
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            anyhow::bail!("Empty datagram");
        }
        Ok(match bytes[0] {
            0 => Self::KeepAlive,
            1 => {
                let modifiers = Modifiers::from_bits_truncate(bytes[1]);
                let keycode = String::from_utf8_lossy(&bytes[2..]).into_owned();
                Self::Keyboard { keycode, modifiers }
            }
            2 => {
                let index = bytes[1];
                let width = u16::from_be_bytes([bytes[2], bytes[3]]);
                let height = u16::from_be_bytes([bytes[4], bytes[5]]);
                Self::ResizeDisplay {
                    index,
                    width,
                    height,
                }
            }
            3 => {
                let count = bytes[1] as usize;
                let mut touches = Vec::with_capacity(count);
                let mut offset = 2;
                for _ in 0..count {
                    let index = bytes[offset];
                    let x = u16::from_be_bytes([bytes[offset + 1], bytes[offset + 2]]);
                    let y = u16::from_be_bytes([bytes[offset + 3], bytes[offset + 4]]);
                    touches.push((index, x, y));
                    offset += 5;
                }
                Self::TouchscreenMulti { touches }
            }
            4 => {
                let index = bytes[1];
                Self::TouchscreenRelease { index }
            }
            5 => Self::Error {
                message: String::from_utf8_lossy(&bytes[1..]).into_owned(),
            },
            d => anyhow::bail!("Invalid datagram discriminant: {d}"),
        })
    }
}
