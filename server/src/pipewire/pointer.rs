use reis::ei;
use reis::ei::button::ButtonState;

use crate::logging::log;

use super::eis::timestamp_us;

pub enum EisPointerEvent {
    Motion { dx: f32, dy: f32 },
}

pub enum EisButtonEvent {
    Button { button: u32, pressed: bool },
}

pub enum EisScrollEvent {
    Scroll { dx: i32, dy: i32 },
}

pub struct MouseState {
    last_pos: Option<(i32, i32)>,
    last_dx: i16,
    last_dy: i16,
    motion_sent: bool,
}

impl MouseState {
    pub fn new() -> Self {
        Self {
            last_pos: None,
            last_dx: 0,
            last_dy: 0,
            motion_sent: false,
        }
    }

    pub fn handle_move(&mut self, dx: i16, dy: i16) -> EisPointerEvent {
        self.last_dx = dx;
        self.last_dy = dy;
        self.motion_sent = true;
        EisPointerEvent::Motion {
            dx: dx as f32,
            dy: dy as f32,
        }
    }

    pub fn update_compositor_pos(&mut self, x: i32, y: i32) -> bool {
        let hit = if let Some((prev_x, prev_y)) = self.last_pos {
            if self.motion_sent {
                // Determine the dominant axis of the last motion and only
                // check whether that axis changed. This tolerates movement
                // along an orthogonal edge while still detecting when the
                // cursor is blocked by the screen boundary on the dominant
                // axis (covers all four edges, not just corners).
                if self.last_dx.unsigned_abs() >= self.last_dy.unsigned_abs() {
                    x == prev_x
                } else {
                    y == prev_y
                }
            } else {
                false
            }
        } else {
            false
        };
        self.last_pos = Some((x, y));
        self.motion_sent = false;
        hit
    }
}

// Linux button codes (input-event-codes.h)
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

pub fn web_button_to_linux(button: u8) -> u32 {
    match button {
        0 => BTN_LEFT,
        1 => BTN_MIDDLE,
        2 => BTN_RIGHT,
        n => 0x100 + n as u32, // other buttons: 0x100+n
    }
}

pub(crate) fn send_pointer_motion(
    connection: &reis::event::Connection,
    device: &reis::event::Device,
    pointer: &ei::Pointer,
    sequence: &mut u32,
    event: EisPointerEvent,
) {
    let serial = connection.serial();
    *sequence = sequence.wrapping_add(1);

    device.device().start_emulating(serial, *sequence);
    match event {
        EisPointerEvent::Motion { dx, dy } => {
            pointer.motion_relative(dx, dy);
        }
    }
    device.device().frame(serial, timestamp_us());
    device.device().stop_emulating(serial);

    if let Err(e) = connection.flush() {
        log(format!("EIS: pointer flush error: {e}"));
    }
}

pub(crate) fn send_button_event(
    connection: &reis::event::Connection,
    device: &reis::event::Device,
    button_dev: &ei::Button,
    sequence: &mut u32,
    event: EisButtonEvent,
) {
    let serial = connection.serial();
    *sequence = sequence.wrapping_add(1);

    device.device().start_emulating(serial, *sequence);
    match event {
        EisButtonEvent::Button { button, pressed } => {
            let state = if pressed {
                ButtonState::Press
            } else {
                ButtonState::Released
            };
            button_dev.button(button, state);
        }
    }
    device.device().frame(serial, timestamp_us());
    device.device().stop_emulating(serial);

    if let Err(e) = connection.flush() {
        log(format!("EIS: button flush error: {e}"));
    }
}

pub(crate) fn send_scroll_event(
    connection: &reis::event::Connection,
    device: &reis::event::Device,
    scroll: &ei::Scroll,
    sequence: &mut u32,
    event: EisScrollEvent,
) {
    let serial = connection.serial();
    *sequence = sequence.wrapping_add(1);

    device.device().start_emulating(serial, *sequence);
    match event {
        EisScrollEvent::Scroll { dx, dy } => {
            scroll.scroll_discrete(dx, dy);
        }
    }
    device.device().frame(serial, timestamp_us());
    device.device().stop_emulating(serial);

    if let Err(e) = connection.flush() {
        log(format!("EIS: scroll flush error: {e}"));
    }
}
