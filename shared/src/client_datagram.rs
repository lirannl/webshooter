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

/// Number of buttons in the standard W3C Gamepad API `buttons` array that we
/// forward. Index order follows the W3C standard gamepad button layout.
pub const GAMEPAD_NUM_BUTTONS: usize = 19;

/// Scale applied to acceleration values (m/s²) when packing them into the
/// `i16` motion fields. ±100 m/s² maps to ±10000, well within `i16`.
pub const MOTION_ACCEL_SCALE: f64 = 100.0;

/// Scale applied to angular-velocity values (deg/s) when packing them into the
/// `i16` motion fields. ±2000 deg/s maps to ±20000, within `i16`.
pub const MOTION_GYRO_SCALE: f64 = 10.0;

/// Motion sensor data for a gamepad, in real-world units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GamepadMotion {
    /// Acceleration in m/s² along each axis.
    pub accel_x: f32,
    pub accel_y: f32,
    pub accel_z: f32,
    /// Angular velocity in deg/s about each axis.
    pub gyro_x: f32,
    pub gyro_y: f32,
    pub gyro_z: f32,
}

/// Convert a client-side gamepad button bitmask (where bit `i` is set when
/// standard button `i` is pressed, see [`GAMEPAD_NUM_BUTTONS`]) into the
/// bitmask expected by inputtino's joypad `set_pressed_buttons`.
///
/// Triggers (L2/R2, bits 6/7) are *not* part of the button bitmask — they are
/// delivered separately as analogue trigger values.
pub fn gamepad_client_buttons_to_inputtino(mask: u32) -> u32 {
    let mut out = 0u32;
    let set = |out: &mut u32, bit: u32, val: u32| {
        if mask & (1 << bit) != 0 {
            *out |= val;
        }
    };
    // A B X Y
    set(&mut out, 0, 0x1000);
    set(&mut out, 1, 0x2000);
    set(&mut out, 2, 0x4000);
    set(&mut out, 3, 0x8000);
    // L1 R1 (shoulders)
    set(&mut out, 4, 0x0100);
    set(&mut out, 5, 0x0200);
    // 6/7 L2/R2 -> triggers, handled separately
    // Select/Back, Start
    set(&mut out, 8, 0x0020);
    set(&mut out, 9, 0x0010);
    // L3/R3 (stick click)
    set(&mut out, 10, 0x0040);
    set(&mut out, 11, 0x0080);
    // DPad
    set(&mut out, 12, 0x0001);
    set(&mut out, 13, 0x0002);
    set(&mut out, 14, 0x0004);
    set(&mut out, 15, 0x0008);
    // Guide/Home
    set(&mut out, 16, 0x0400);
    // Touchpad / Misc extra buttons
    set(&mut out, 17, 0x100000);
    set(&mut out, 18, 0x200000);
    out
}

#[named_constants(preserve_original)]
#[repr(u8)]
#[derive(Debug, Clone, PartialEq)]
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
    /// A full snapshot of a gamepad's state. The virtual device is created
    /// lazily on the server the first time a `Gamepad` datagram arrives for a
    /// given `id`, so captures that never see gamepad input never create one.
    Gamepad {
        id: u8,
        /// Client-side button bitmask, bit `i` set when standard button `i`
        /// is pressed (see [`GAMEPAD_NUM_BUTTONS`]).
        buttons: u32,
        /// Left stick, range -32768..=32767.
        lx: i16,
        ly: i16,
        /// Right stick, range -32768..=32767.
        rx: i16,
        ry: i16,
        /// Left trigger, range 0..=32767.
        lt: i16,
        /// Right trigger, range 0..=32767.
        rt: i16,
        /// Motion data (accelerometer + gyroscope). `None` when no motion
        /// source is available on the client.
        motion: Option<GamepadMotion>,
    },
    /// Tear down the virtual gamepad device for `id` (e.g. controller
    /// disconnected on the client).
    GamepadDisconnect {
        id: u8,
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
            Self::Gamepad {
                id,
                buttons,
                lx,
                ly,
                rx,
                ry,
                lt,
                rt,
                motion,
            } => {
                let mut buf = Vec::with_capacity(33);
                buf.push(ClientDatagramVariants::GAMEPAD.0);
                buf.push(*id);
                buf.extend_from_slice(&buttons.to_be_bytes());
                buf.extend_from_slice(&lx.to_be_bytes());
                buf.extend_from_slice(&ly.to_be_bytes());
                buf.extend_from_slice(&rx.to_be_bytes());
                buf.extend_from_slice(&ry.to_be_bytes());
                buf.extend_from_slice(&lt.to_be_bytes());
                buf.extend_from_slice(&rt.to_be_bytes());
                match motion {
                    None => {
                        buf.push(0);
                    }
                    Some(m) => {
                        buf.push(1);
                        let ax = (m.accel_x as f64 * MOTION_ACCEL_SCALE)
                            .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                        let ay = (m.accel_y as f64 * MOTION_ACCEL_SCALE)
                            .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                        let az = (m.accel_z as f64 * MOTION_ACCEL_SCALE)
                            .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                        let gx = (m.gyro_x as f64 * MOTION_GYRO_SCALE)
                            .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                        let gy = (m.gyro_y as f64 * MOTION_GYRO_SCALE)
                            .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                        let gz = (m.gyro_z as f64 * MOTION_GYRO_SCALE)
                            .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                        buf.extend_from_slice(&ax.to_be_bytes());
                        buf.extend_from_slice(&ay.to_be_bytes());
                        buf.extend_from_slice(&az.to_be_bytes());
                        buf.extend_from_slice(&gx.to_be_bytes());
                        buf.extend_from_slice(&gy.to_be_bytes());
                        buf.extend_from_slice(&gz.to_be_bytes());
                    }
                }
                buf
            }
            Self::GamepadDisconnect { id } => {
                vec![ClientDatagramVariants::GAMEPAD_DISCONNECT.0, *id]
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
            ClientDatagramVariants::GAMEPAD => {
                if bytes.len() < 19 {
                    anyhow::bail!("Gamepad datagram too short: {} bytes", bytes.len());
                }
                let id = bytes[1];
                let buttons = u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
                let lx = i16::from_be_bytes([bytes[6], bytes[7]]);
                let ly = i16::from_be_bytes([bytes[8], bytes[9]]);
                let rx = i16::from_be_bytes([bytes[10], bytes[11]]);
                let ry = i16::from_be_bytes([bytes[12], bytes[13]]);
                let lt = i16::from_be_bytes([bytes[14], bytes[15]]);
                let rt = i16::from_be_bytes([bytes[16], bytes[17]]);
                let motion = if bytes.len() >= 33 && bytes[18] != 0 {
                    let accel_x = i16::from_be_bytes([bytes[19], bytes[20]]) as f64 / MOTION_ACCEL_SCALE;
                    let accel_y = i16::from_be_bytes([bytes[21], bytes[22]]) as f64 / MOTION_ACCEL_SCALE;
                    let accel_z = i16::from_be_bytes([bytes[23], bytes[24]]) as f64 / MOTION_ACCEL_SCALE;
                    let gyro_x = i16::from_be_bytes([bytes[25], bytes[26]]) as f64 / MOTION_GYRO_SCALE;
                    let gyro_y = i16::from_be_bytes([bytes[27], bytes[28]]) as f64 / MOTION_GYRO_SCALE;
                    let gyro_z = i16::from_be_bytes([bytes[29], bytes[30]]) as f64 / MOTION_GYRO_SCALE;
                    Some(GamepadMotion {
                        accel_x: accel_x as f32,
                        accel_y: accel_y as f32,
                        accel_z: accel_z as f32,
                        gyro_x: gyro_x as f32,
                        gyro_y: gyro_y as f32,
                        gyro_z: gyro_z as f32,
                    })
                } else {
                    None
                };
                Self::Gamepad {
                    id,
                    buttons,
                    lx,
                    ly,
                    rx,
                    ry,
                    lt,
                    rt,
                    motion,
                }
            }
            ClientDatagramVariants::GAMEPAD_DISCONNECT => {
                let id = bytes[1];
                Self::GamepadDisconnect { id }
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
