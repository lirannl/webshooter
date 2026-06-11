use anyhow::Result;
use bitflags::bitflags;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Modifiers: u8 {
        const SHIFT = 1;
        const CTRL  = 2;
        const ALT   = 4;
        const META  = 8;
    }
}

#[derive(Clone)]
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
    Touchscreen {
        x: u16,
        y: u16,
        index: u8,
    },
    TouchscreenRelease {
        index: Option<u8>,
    },
}

impl ClientDatagram {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() == 0 {
            return Err(anyhow::anyhow!("Empty datagram"));
        }
        Ok(match bytes[0] {
            0 => Self::KeepAlive,
            1 => {
                let modifiers = Modifiers::from_bits_truncate(bytes[1]);
                let keycode = String::from_utf8_lossy(&bytes[2..]);
                Self::Keyboard {
                    keycode: keycode.to_string(),
                    modifiers,
                }
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
                let x = u16::from_be_bytes([bytes[1], bytes[2]]);
                let y = u16::from_be_bytes([bytes[3], bytes[4]]);
                let index = bytes[5];
                Self::Touchscreen { x, y, index }
            }
            4 => {
                let index = Some(bytes[1]);
                Self::TouchscreenRelease { index }
            }
            _ => return Err(anyhow::anyhow!("Invalid datagram type")),
        })
    }
}
