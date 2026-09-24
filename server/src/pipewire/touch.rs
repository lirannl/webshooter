use std::collections::HashSet;

use reis::ei;


use super::eis::with_emulation;

pub enum EisTouchEvent {
    Down { x: u16, y: u16, index: u8 },
    Motion { x: u16, y: u16, index: u8 },
    Up { index: u8 },
}

pub struct TouchState {
    active_slots: HashSet<u8>,
}

impl TouchState {
    pub fn new() -> Self {
        Self {
            active_slots: Default::default(),
        }
    }

    pub fn handle_touch(&mut self, index: u8, x: u16, y: u16) -> Vec<EisTouchEvent> {
        let is_new = self.active_slots.insert(index);
        vec![if is_new {
            EisTouchEvent::Down { x, y, index }
        } else {
            EisTouchEvent::Motion { x, y, index }
        }]
    }

    pub fn handle_release(&mut self, index: u8) -> Option<EisTouchEvent> {
        if self.active_slots.remove(&index) {
            Some(EisTouchEvent::Up { index })
        } else {
            None
        }
    }

    pub fn release_all(&mut self) -> Vec<EisTouchEvent> {
        self.active_slots
            .drain()
            .map(|index| EisTouchEvent::Up { index })
            .collect()
    }
}

pub(crate) fn offset_touch_event(event: EisTouchEvent, (ox, oy): (i32, i32)) -> EisTouchEvent {
    match event {
        EisTouchEvent::Down { x, y, index } => EisTouchEvent::Down {
            x: (x as i32 + ox).max(0).min(u16::MAX as i32) as u16,
            y: (y as i32 + oy).max(0).min(u16::MAX as i32) as u16,
            index,
        },
        EisTouchEvent::Motion { x, y, index } => EisTouchEvent::Motion {
            x: (x as i32 + ox).max(0).min(u16::MAX as i32) as u16,
            y: (y as i32 + oy).max(0).min(u16::MAX as i32) as u16,
            index,
        },
        EisTouchEvent::Up { .. } => event,
    }
}

pub(crate) fn send_touch_event(
    connection: &reis::event::Connection,
    device: &reis::event::Device,
    touchscreen: &ei::Touchscreen,
    sequence: &mut u32,
    event: EisTouchEvent,
) {
    with_emulation(connection, device, sequence, "touchscreen", || match event {
        EisTouchEvent::Down { x, y, index } => {
            touchscreen.down(index as u32, x as f32, y as f32);
        }
        EisTouchEvent::Motion { x, y, index } => {
            touchscreen.motion(index as u32, x as f32, y as f32);
        }
        EisTouchEvent::Up { index } => {
            touchscreen.up(index as u32);
        }
    });
}
