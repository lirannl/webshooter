use crate::codec::Codec;
use anyhow::Result;
use named_constants::named_constants;

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Modifiers: u8 {
        const SHIFT = 1;
        const CTRL  = 2;
        const ALT   = 4;
        const META  = 8;
    }
}

#[named_constants(preserve_original)]
#[repr(u8)]
#[derive(Debug, Clone, PartialEq, Eq)]
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
        index: u8,
        x: u16,
        y: u16,
    },
    TouchscreenRelease {
        index: u8,
    },
    Error {
        message: String,
    },
    DecoderCapabilities {
        decoders: Vec<Codec>,
    },
    MouseMove {
        dx: i16,
        dy: i16,
    },
    MouseButton {
        button: u8,
        pressed: bool,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
}

impl ClientDatagram {
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::KeepAlive => vec![ClientDatagramVariants::KEEP_ALIVE.0],
            Self::Keyboard { keycode, modifiers } => {
                let key_bytes = keycode.as_bytes();
                let mut buf = Vec::with_capacity(2 + key_bytes.len());
                buf.push(ClientDatagramVariants::KEYBOARD.0);
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
                buf.push(ClientDatagramVariants::RESIZE_DISPLAY.0);
                buf.push(*index);
                buf.extend_from_slice(&width.to_be_bytes());
                buf.extend_from_slice(&height.to_be_bytes());
                buf
            }
            Self::Touchscreen { index, x, y } => {
                let mut buf = Vec::with_capacity(6);
                buf.push(ClientDatagramVariants::TOUCHSCREEN.0);
                buf.extend_from_slice(&x.to_be_bytes());
                buf.extend_from_slice(&y.to_be_bytes());
                buf.push(*index);
                buf
            }
            Self::TouchscreenRelease { index } => {
                vec![ClientDatagramVariants::TOUCHSCREEN_RELEASE.0, *index]
            }
            Self::Error { message } => {
                let mut buf = Vec::with_capacity(message.len() + 1);
                buf.push(ClientDatagramVariants::ERROR.0);
                buf.extend_from_slice(message.as_bytes());
                buf
            }
            Self::DecoderCapabilities { decoders } => {
                let mut buf = Vec::with_capacity(2 + decoders.len());
                buf.push(ClientDatagramVariants::DECODER_CAPABILITIES.0);
                buf.push(decoders.len() as u8);
                for codec in decoders {
                    buf.push(codec.to_byte());
                }
                buf
            }
            Self::MouseMove { dx, dy } => {
                let mut buf = Vec::with_capacity(5);
                buf.push(ClientDatagramVariants::MOUSE_MOVE.0);
                buf.extend_from_slice(&dx.to_be_bytes());
                buf.extend_from_slice(&dy.to_be_bytes());
                buf
            }
            Self::MouseButton { button, pressed } => {
                vec![
                    ClientDatagramVariants::MOUSE_BUTTON.0,
                    *button,
                    *pressed as u8,
                ]
            }
            Self::Scroll { dx, dy } => {
                let mut buf = Vec::with_capacity(9);
                buf.push(ClientDatagramVariants::SCROLL.0);
                buf.extend_from_slice(&dx.to_be_bytes());
                buf.extend_from_slice(&dy.to_be_bytes());
                buf
            }
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            anyhow::bail!("Empty datagram");
        }
        Ok(match ClientDatagramVariants(bytes[0]) {
            ClientDatagramVariants::KEEP_ALIVE => Self::KeepAlive,
            ClientDatagramVariants::KEYBOARD => {
                let modifiers = Modifiers::from_bits_truncate(bytes[1]);
                let keycode = String::from_utf8_lossy(&bytes[2..]).into_owned();
                Self::Keyboard { keycode, modifiers }
            }
            ClientDatagramVariants::RESIZE_DISPLAY => {
                let index = bytes[1];
                let width = u16::from_be_bytes([bytes[2], bytes[3]]);
                let height = u16::from_be_bytes([bytes[4], bytes[5]]);
                Self::ResizeDisplay {
                    index,
                    width,
                    height,
                }
            }
            ClientDatagramVariants::TOUCHSCREEN => {
                let x = u16::from_be_bytes([bytes[1], bytes[2]]);
                let y = u16::from_be_bytes([bytes[3], bytes[4]]);
                let index = bytes[5];
                Self::Touchscreen { x, y, index }
            }
            ClientDatagramVariants::TOUCHSCREEN_RELEASE => {
                let index = bytes[1];
                Self::TouchscreenRelease { index }
            }
            ClientDatagramVariants::ERROR => Self::Error {
                message: String::from_utf8_lossy(&bytes[1..]).into_owned(),
            },
            ClientDatagramVariants::DECODER_CAPABILITIES => {
                let len = bytes[1] as usize;
                let decoders = (0..len)
                    .filter_map(|i| Codec::from_byte(bytes[2 + i]).ok())
                    .collect();
                Self::DecoderCapabilities { decoders }
            }
            ClientDatagramVariants::MOUSE_MOVE => {
                let dx = i16::from_be_bytes([bytes[1], bytes[2]]);
                let dy = i16::from_be_bytes([bytes[3], bytes[4]]);
                Self::MouseMove { dx, dy }
            }
            ClientDatagramVariants::MOUSE_BUTTON => {
                let button = bytes[1];
                let pressed = bytes[2] != 0;
                Self::MouseButton { button, pressed }
            }
            ClientDatagramVariants::SCROLL => {
                let dx = i32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
                let dy = i32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]);
                Self::Scroll { dx, dy }
            }
            n => anyhow::bail!("Invalid datagram discriminant: {}", n.0),
        })
    }
}
