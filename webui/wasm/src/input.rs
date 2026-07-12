use crate::with_wt;
use shared::client_datagram::{ClientDatagram, Modifiers};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::{Element, HtmlCanvasElement, KeyboardEvent, TouchEvent};

pub fn setup_keyboard(canvas: &HtmlCanvasElement) {
    canvas.set_tab_index(0);
    let _ = canvas.focus();

    let keydown_cb = Closure::wrap(Box::new(move |e: KeyboardEvent| {
        e.prevent_default();
        let mut modifiers = Modifiers::empty();
        if e.shift_key() {
            modifiers |= Modifiers::SHIFT;
        }
        if e.ctrl_key() {
            modifiers |= Modifiers::CTRL;
        }
        if e.alt_key() {
            modifiers |= Modifiers::ALT;
        }
        if e.meta_key() {
            modifiers |= Modifiers::META;
        }
        let msg = ClientDatagram::Keyboard {
            keycode: e.code(),
            modifiers,
        };
        let bytes = msg.to_bytes();
        let buf = js_sys::Uint8Array::from(&bytes[..]);
        with_wt(|gwt| {
            let _ = gwt.writer.write_with_chunk(buf.as_ref());
        });
    }) as Box<dyn FnMut(KeyboardEvent)>);
    let _ = canvas.add_event_listener_with_callback("keydown", keydown_cb.as_ref().unchecked_ref());
    keydown_cb.forget();
}

fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

pub fn setup_touch(canvas: &HtmlCanvasElement) {
    let _ = canvas.style().set_property("touch-action", "none");

    let slots = Rc::new(RefCell::new(HashMap::<i32, u8>::new()));
    let free_slots = Rc::new(RefCell::new(Vec::<u8>::new()));
    let next_slot = Rc::new(RefCell::new(0u8));

    let canvas_press = canvas.clone();
    let slots_press = slots.clone();
    let free_press = free_slots.clone();
    let next_press = next_slot.clone();

    let press_cb = Closure::wrap(Box::new(move |e: TouchEvent| {
        e.prevent_default();
        let rect = canvas_press
            .unchecked_ref::<Element>()
            .get_bounding_client_rect();
        let cw = canvas_press.width() as f64;
        let ch = canvas_press.height() as f64;
        if rect.width() == 0.0 || rect.height() == 0.0 || cw == 0.0 || ch == 0.0 {
            return;
        }
        let touches = e.changed_touches();
        let mut touches_vec = Vec::new();
        for i in 0..touches.length() {
            let touch = touches.item(i).unwrap();
            let id = touch.identifier();
            let slot = {
                let mut slots = slots_press.borrow_mut();
                let mut free = free_press.borrow_mut();
                let mut next = next_press.borrow_mut();
                *slots.entry(id).or_insert_with(|| {
                    free.pop().unwrap_or_else(|| {
                        let s = *next;
                        *next = next.wrapping_add(1);
                        s
                    })
                })
            };
            let nx = (touch.client_x() as f64 - rect.left()) / rect.width();
            let ny = (touch.client_y() as f64 - rect.top()) / rect.height();
            let x = clamp((nx * cw).round(), 0.0, cw - 1.0) as u16;
            let y = clamp((ny * ch).round(), 0.0, ch - 1.0) as u16;
            touches_vec.push((slot, x, y));
        }
        for (slot, x, y) in touches_vec {
            let msg = ClientDatagram::Touchscreen { index: slot, x, y };
            let bytes = msg.to_bytes();
            let buf = js_sys::Uint8Array::from(&bytes[..]);
            with_wt(|gwt| {
                let _ = gwt.writer.write_with_chunk(buf.as_ref());
            });
        }
    }) as Box<dyn FnMut(TouchEvent)>);
    let _ =
        canvas.add_event_listener_with_callback("touchstart", press_cb.as_ref().unchecked_ref());
    let _ = canvas.add_event_listener_with_callback("touchmove", press_cb.as_ref().unchecked_ref());
    press_cb.forget();

    let slots_rel = slots.clone();
    let free_rel = free_slots.clone();
    let release_cb = Closure::wrap(Box::new(move |e: TouchEvent| {
        e.prevent_default();
        let touches = e.changed_touches();
        let mut release_vec = Vec::new();
        for i in 0..touches.length() {
            let touch = touches.item(i).unwrap();
            let id = touch.identifier();
            if let Some(slot) = slots_rel.borrow_mut().remove(&id) {
                free_rel.borrow_mut().push(slot);
                release_vec.push(slot);
            }
        }
        if !release_vec.is_empty() {
            // Also send individual releases for server slot cleanup
            for slot in release_vec {
                let msg = ClientDatagram::TouchscreenRelease { index: slot };
                let bytes = msg.to_bytes();
                let buf = js_sys::Uint8Array::from(&bytes[..]);
                with_wt(|gwt| {
                    let _ = gwt.writer.write_with_chunk(buf.as_ref());
                });
            }
        }
    }) as Box<dyn FnMut(TouchEvent)>);
    let _ =
        canvas.add_event_listener_with_callback("touchend", release_cb.as_ref().unchecked_ref());
    let _ =
        canvas.add_event_listener_with_callback("touchcancel", release_cb.as_ref().unchecked_ref());
    release_cb.forget();
}
