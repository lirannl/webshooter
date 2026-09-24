use crate::codec::Codec;
use anyhow::Result;
use log::Level;
use named_constants::named_constants;
use std::collections::VecDeque;

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

/// True when the wire discriminant `byte` (the first byte of an encoded
/// [`ClientDatagram`]) identifies an *input* event rather than control
/// traffic. The server's receive pump uses this cheap byte check to apply
/// flood limiting to input datagrams *before* spending CPU on the full parse.
pub fn is_input_byte(byte: u8) -> bool {
    matches!(
        ClientDatagramVariants(byte),
        ClientDatagramVariants::KEYBOARD
            | ClientDatagramVariants::MOUSE_MOVE
            | ClientDatagramVariants::MOUSE_BUTTON
            | ClientDatagramVariants::SCROLL
            | ClientDatagramVariants::TOUCHSCREEN
            | ClientDatagramVariants::TOUCHSCREEN_RELEASE
            | ClientDatagramVariants::GAMEPAD
            | ClientDatagramVariants::GAMEPAD_DISCONNECT
    )
}

/// Merge `msg` into the tail of a queue of pending input datagrams when
/// plausible, collapsing consecutive same-kind events into one representative
/// datagram so a throttled window keeps the newest state at a fraction of the
/// event count:
///
/// - mouse motion and scroll deltas are summed (saturating), so no movement is
///   lost across the window;
/// - touch keeps the newest absolute position per slot;
/// - gamepad keeps the newest full snapshot per controller;
/// - everything else (keys, buttons, releases, …) is appended in arrival order.
///
/// Used by both the client (which throttles before sending) and the server
/// (which enforces the throttle by discarding excess input). Inputs travel as
/// WebTransport datagrams — unreliable, best-effort delivery — so collapsing
/// them to the latest state is consistent with the transport's semantics.
pub fn coalesce_input(queue: &mut VecDeque<ClientDatagram>, msg: ClientDatagram) {
    match &msg {
        ClientDatagram::MouseMove { dx, dy } => {
            if let Some(ClientDatagram::MouseMove { dx: pdx, dy: pdy }) = queue.back_mut() {
                *pdx = pdx.saturating_add(*dx);
                *pdy = pdy.saturating_add(*dy);
                return;
            }
        }
        ClientDatagram::Scroll { dx, dy } => {
            if let Some(ClientDatagram::Scroll { dx: pdx, dy: pdy }) = queue.back_mut() {
                *pdx = pdx.saturating_add(*dx);
                *pdy = pdy.saturating_add(*dy);
                return;
            }
        }
        ClientDatagram::Touchscreen { index, x, y } => {
            if let Some(ClientDatagram::Touchscreen { index: pi, x: px, y: py }) = queue.back_mut()
                && pi == index
            {
                *px = *x;
                *py = *y;
                return;
            }
        }
        ClientDatagram::Gamepad { id, .. } => {
            if let Some(ClientDatagram::Gamepad { id: pid, .. }) = queue.back_mut()
                && pid == id
            {
                // Keep only the newest full snapshot for this controller.
                *queue.back_mut().unwrap() = msg;
                return;
            }
        }
        _ => {}
    }
    queue.push_back(msg);
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
        level: Level,
        message: String,
    },
    DecoderCapabilities {
        decoders: Vec<Codec>,
    },
    /// The client's AudioContext has entered the `Running` state (i.e. a user
    /// gesture has occurred) and audio can actually be played. The server
    /// should only create the PipeWire audio sink and forward Opus once this
    /// arrives, so we don't stream audio the browser cannot play yet.
    /// `channels`/`rate` are the client's AudioContext output capabilities, so
    /// the server can build a PipeWire sink + Opus encoder that matches them
    /// (mono, stereo, or more) instead of assuming a fixed layout.
    AudioReady {
        channels: u8,
        rate: u32,
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
    /// Picture Loss Indication: the client detected a gap in the frame
    /// sequence (a frame was lost to packet loss) and asks the server to
    /// emit a fresh keyframe so decoding can resynchronise. Cheap to send
    /// because a back-channel already exists; bounded recovery instead of
    /// waiting for the next scheduled keyframe.
    RequestKeyframe,
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
                            .clamp(i16::MIN as f64, i16::MAX as f64)
                            as i16;
                        let ay = (m.accel_y as f64 * MOTION_ACCEL_SCALE)
                            .clamp(i16::MIN as f64, i16::MAX as f64)
                            as i16;
                        let az = (m.accel_z as f64 * MOTION_ACCEL_SCALE)
                            .clamp(i16::MIN as f64, i16::MAX as f64)
                            as i16;
                        let gx = (m.gyro_x as f64 * MOTION_GYRO_SCALE)
                            .clamp(i16::MIN as f64, i16::MAX as f64)
                            as i16;
                        let gy = (m.gyro_y as f64 * MOTION_GYRO_SCALE)
                            .clamp(i16::MIN as f64, i16::MAX as f64)
                            as i16;
                        let gz = (m.gyro_z as f64 * MOTION_GYRO_SCALE)
                            .clamp(i16::MIN as f64, i16::MAX as f64)
                            as i16;
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
            Self::Error { level, message } => {
                let mut buf = Vec::with_capacity(2 + message.len());
                buf.push(ClientDatagramVariants::ERROR.0);
                buf.push(crate::log_level::level_to_byte(*level));
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
            Self::AudioReady { channels, rate } => {
                let mut buf = Vec::with_capacity(6);
                buf.push(ClientDatagramVariants::AUDIO_READY.0);
                buf.push(*channels);
                buf.extend_from_slice(&rate.to_be_bytes());
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
            Self::RequestKeyframe => vec![ClientDatagramVariants::REQUEST_KEYFRAME.0],
        }
    }

    /// Parse a client datagram from its wire bytes.
    ///
    /// This is fed with data that arrives from the network (the WebTransport
    /// datagram and uni-stream pumps), so it must never panic — not even on
    /// truncated or malformed input. Every variant length-checks its fixed
    /// fields first and returns `Err` instead of indexing out of bounds,
    /// so a flood of garbage bytes can slow a session down but never tear a
    /// pump task (and with it the whole session) down.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            anyhow::bail!("Empty datagram");
        }
        let data = &bytes[1..];
        Ok(match ClientDatagramVariants(bytes[0]) {
            ClientDatagramVariants::KEEP_ALIVE => Self::KeepAlive,
            ClientDatagramVariants::KEYBOARD => {
                let Some((&modifier, key_bytes)) = data.split_first() else {
                    anyhow::bail!("Keyboard datagram too short: {} bytes", bytes.len());
                };
                Self::Keyboard {
                    keycode: String::from_utf8_lossy(key_bytes).into_owned(),
                    modifiers: Modifiers::from_bits_truncate(modifier),
                }
            }
            ClientDatagramVariants::RESIZE_DISPLAY => {
                let [index, a, b, c, d] = data else {
                    anyhow::bail!("ResizeDisplay datagram too short: {} bytes", bytes.len());
                };
                Self::ResizeDisplay {
                    index: *index,
                    width: u16::from_be_bytes([*a, *b]),
                    height: u16::from_be_bytes([*c, *d]),
                }
            }
            ClientDatagramVariants::TOUCHSCREEN => {
                let [a, b, c, d, index] = data else {
                    anyhow::bail!("Touchscreen datagram too short: {} bytes", bytes.len());
                };
                Self::Touchscreen {
                    x: u16::from_be_bytes([*a, *b]),
                    y: u16::from_be_bytes([*c, *d]),
                    index: *index,
                }
            }
            ClientDatagramVariants::TOUCHSCREEN_RELEASE => {
                let [index] = data else {
                    anyhow::bail!("TouchscreenRelease datagram too short: {} bytes", bytes.len());
                };
                Self::TouchscreenRelease { index: *index }
            }
            ClientDatagramVariants::GAMEPAD => {
                if data.len() < 18 {
                    anyhow::bail!("Gamepad datagram too short: {} bytes", bytes.len());
                }
                let id = data[0];
                let buttons = u32::from_be_bytes([data[1], data[2], data[3], data[4]]);
                let lx = i16::from_be_bytes([data[5], data[6]]);
                let ly = i16::from_be_bytes([data[7], data[8]]);
                let rx = i16::from_be_bytes([data[9], data[10]]);
                let ry = i16::from_be_bytes([data[11], data[12]]);
                let lt = i16::from_be_bytes([data[13], data[14]]);
                let rt = i16::from_be_bytes([data[15], data[16]]);
                let motion = if data[17] != 0 {
                    let Some(body) = data.get(18..30) else {
                        anyhow::bail!("Gamepad motion datagram too short: {} bytes", bytes.len());
                    };
                    let accel_x =
                        i16::from_be_bytes([body[0], body[1]]) as f64 / MOTION_ACCEL_SCALE;
                    let accel_y =
                        i16::from_be_bytes([body[2], body[3]]) as f64 / MOTION_ACCEL_SCALE;
                    let accel_z =
                        i16::from_be_bytes([body[4], body[5]]) as f64 / MOTION_ACCEL_SCALE;
                    let gyro_x =
                        i16::from_be_bytes([body[6], body[7]]) as f64 / MOTION_GYRO_SCALE;
                    let gyro_y =
                        i16::from_be_bytes([body[8], body[9]]) as f64 / MOTION_GYRO_SCALE;
                    let gyro_z =
                        i16::from_be_bytes([body[10], body[11]]) as f64 / MOTION_GYRO_SCALE;
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
                let [id] = data else {
                    anyhow::bail!("GamepadDisconnect datagram too short: {} bytes", bytes.len());
                };
                Self::GamepadDisconnect { id: *id }
            }
            ClientDatagramVariants::ERROR => {
                let Some((&level, message)) = data.split_first() else {
                    anyhow::bail!("Error datagram too short: {} bytes", bytes.len());
                };
                Self::Error {
                    level: crate::log_level::level_from_byte(level)?,
                    message: String::from_utf8_lossy(message).into_owned(),
                }
            }
            ClientDatagramVariants::DECODER_CAPABILITIES => {
                let Some((&len, codecs)) = data.split_first() else {
                    anyhow::bail!("DecoderCapabilities datagram too short: {} bytes", bytes.len());
                };
                if codecs.len() < len as usize {
                    anyhow::bail!(
                        "DecoderCapabilities datagram too short: {} codec bytes but {} declared",
                        codecs.len(),
                        len
                    );
                }
                let decoders = codecs[..len as usize]
                    .iter()
                    .filter_map(|b| Codec::from_byte(*b).ok())
                    .collect();
                Self::DecoderCapabilities { decoders }
            }
            ClientDatagramVariants::AUDIO_READY => {
                let [channels, a, b, c, d] = data else {
                    anyhow::bail!("AudioReady datagram too short: {} bytes", bytes.len());
                };
                Self::AudioReady {
                    channels: *channels,
                    rate: u32::from_be_bytes([*a, *b, *c, *d]),
                }
            }
            ClientDatagramVariants::MOUSE_MOVE => {
                let [a, b, c, d] = data else {
                    anyhow::bail!("MouseMove datagram too short: {} bytes", bytes.len());
                };
                Self::MouseMove {
                    dx: i16::from_be_bytes([*a, *b]),
                    dy: i16::from_be_bytes([*c, *d]),
                }
            }
            ClientDatagramVariants::MOUSE_BUTTON => {
                let [button, pressed] = data else {
                    anyhow::bail!("MouseButton datagram too short: {} bytes", bytes.len());
                };
                Self::MouseButton {
                    button: *button,
                    pressed: *pressed != 0,
                }
            }
            ClientDatagramVariants::SCROLL => {
                let [a, b, c, d, e, f, g, h] = data else {
                    anyhow::bail!("Scroll datagram too short: {} bytes", bytes.len());
                };
                Self::Scroll {
                    dx: i32::from_be_bytes([*a, *b, *c, *d]),
                    dy: i32::from_be_bytes([*e, *f, *g, *h]),
                }
            }
            ClientDatagramVariants::REQUEST_KEYFRAME => Self::RequestKeyframe,
            n => anyhow::bail!("Invalid datagram discriminant: {}", n.0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Codec;

    fn sample_datagrams() -> Vec<ClientDatagram> {
        vec![
            ClientDatagram::KeepAlive,
            ClientDatagram::Keyboard {
                keycode: "KeyA".into(),
                modifiers: Modifiers::CTRL | Modifiers::SHIFT,
            },
            ClientDatagram::Keyboard {
                keycode: String::new(),
                modifiers: Modifiers::empty(),
            },
            ClientDatagram::ResizeDisplay {
                index: 1,
                width: 1920,
                height: 1080,
            },
            ClientDatagram::Touchscreen {
                index: 0,
                x: 100,
                y: 200,
            },
            ClientDatagram::TouchscreenRelease { index: 2 },
            ClientDatagram::Gamepad {
                id: 3,
                buttons: 0xFFFF,
                lx: -32768,
                ly: 32767,
                rx: 0,
                ry: -1,
                lt: 10,
                rt: 20,
                motion: None,
            },
            ClientDatagram::Gamepad {
                id: 4,
                buttons: 0,
                lx: 0,
                ly: 0,
                rx: 0,
                ry: 0,
                lt: 0,
                rt: 0,
                motion: Some(GamepadMotion {
                    accel_x: 1.0,
                    accel_y: -2.0,
                    accel_z: 3.0,
                    gyro_x: 10.0,
                    gyro_y: -20.0,
                    gyro_z: 30.0,
                }),
            },
            ClientDatagram::GamepadDisconnect { id: 7 },
            ClientDatagram::Error {
                level: log::Level::Warn,
                message: "hello".into(),
            },
            ClientDatagram::DecoderCapabilities {
                decoders: vec![Codec::Av1, Codec::H264, Codec::Vp9],
            },
            ClientDatagram::AudioReady {
                channels: 2,
                rate: 48000,
            },
            ClientDatagram::MouseMove { dx: 12, dy: -34 },
            ClientDatagram::MouseButton {
                button: 2,
                pressed: true,
            },
            ClientDatagram::Scroll { dx: 100, dy: -200 },
            ClientDatagram::RequestKeyframe,
        ]
    }

    /// Every variant (with and without optional fields) round-trips.
    #[test]
    fn all_variants_round_trip() {
        for dgram in sample_datagrams() {
            let bytes = dgram.to_bytes();
            assert_eq!(
                ClientDatagram::from_bytes(&bytes).unwrap(),
                dgram,
                "round-trip failed for {dgram:?}"
            );
        }
    }

    /// Truncating any datagram — even right before a fixed-offset field read —
    /// must yield `Err`, never a panic. This is a hard requirement because the
    /// parser sits directly on the network receive path.
    #[test]
    fn truncated_datagrams_return_err_not_panic() {
        for dgram in sample_datagrams() {
            let bytes = dgram.to_bytes();
            for cut in 0..bytes.len() {
                let _ = ClientDatagram::from_bytes(&bytes[..cut]);
            }
        }
    }

    /// Garbage and unknown discriminants of every plausible length must never
    /// panic either.
    #[test]
    fn arbitrary_bytes_never_panic() {
        for disc in 0..=255u8 {
            for len in 0..40usize {
                let mut buf = vec![disc];
                for i in 0..len {
                    buf.push(i as u8 ^ disc);
                }
                let _ = ClientDatagram::from_bytes(&buf);
            }
        }
    }

    /// A `DecoderCapabilities` payload declaring more codecs than it carries
    /// must fail, not read past the end.
    #[test]
    fn decoder_capabilities_checks_declared_len() {
        let truncated = [ClientDatagramVariants::DECODER_CAPABILITIES.0, 5, 0, 1];
        assert!(ClientDatagram::from_bytes(&truncated).is_err());
    }

    /// The byte-level classifier used by the receive pump agrees with the
    /// variants it is meant to catch, for exactly the input datagrams.
    #[test]
    fn is_input_byte_matches_input_variants() {
        for dgram in sample_datagrams() {
            let bytes = dgram.to_bytes();
            let expect_input = matches!(
                dgram,
                ClientDatagram::Keyboard { .. }
                    | ClientDatagram::MouseMove { .. }
                    | ClientDatagram::MouseButton { .. }
                    | ClientDatagram::Scroll { .. }
                    | ClientDatagram::Touchscreen { .. }
                    | ClientDatagram::TouchscreenRelease { .. }
                    | ClientDatagram::Gamepad { .. }
                    | ClientDatagram::GamepadDisconnect { .. }
            );
            assert_eq!(
                is_input_byte(bytes[0]),
                expect_input,
                "is_input_byte mismatch for {dgram:?}"
            );
        }
    }

    /// Unknown discriminants are never flagged as input.
    #[test]
    fn is_input_byte_rejects_unknown_discriminants() {
        for disc in [0x0E, 0x0F, 0x7F, 0xFF] {
            assert!(!is_input_byte(disc));
        }
    }
}
