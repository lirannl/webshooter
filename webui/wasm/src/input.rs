use crate::with_wt;
use shared::client_datagram::{ClientDatagram, Modifiers};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::{Element, HtmlCanvasElement, KeyboardEvent, MouseEvent, PointerEvent, TouchEvent};

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

pub fn setup_mouse(canvas: &HtmlCanvasElement, release_flag: Rc<Cell<bool>>) {
    let window = web_sys::window().unwrap();
    let document = window.document().unwrap();
    let performance = window.performance().unwrap();

    // State machine:
    //   can_capture     – true when allowed to request pointer lock
    //   awaiting_exit   – true after server-initiated release, waiting for
    //                     1 pointerenter + 1 pointerleave before re-capturing
    //   locked_at       – performance.now() when lock was acquired; server
    //                     releases are ignored for 1 s after this
    let can_capture = Rc::new(Cell::new(true));
    let awaiting_exit = Rc::new(Cell::new(false));
    let entered_once = Rc::new(Cell::new(false));
    let locked_at = Rc::new(Cell::new(0.0f64));

    // --- pointerenter: request lock if allowed (mouse only) ---
    {
        let can_capture = can_capture.clone();
        let awaiting_exit = awaiting_exit.clone();
        let entered_once = entered_once.clone();
        let cb = Closure::wrap(Box::new(move |e: PointerEvent| {
            if e.pointer_type() != "mouse" {
                return;
            }
            if awaiting_exit.get() {
                entered_once.set(true);
                return;
            }
            if can_capture.get() {
                let _ = canvas.request_pointer_lock();
            }
        }) as Box<dyn FnMut(PointerEvent)>);
        let _ =
            canvas.add_event_listener_with_callback("pointerenter", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // --- pointerleave: complete the re-entry cycle (mouse only) ---
    {
        let can_capture = can_capture.clone();
        let awaiting_exit = awaiting_exit.clone();
        let entered_once = entered_once.clone();
        let cb = Closure::wrap(Box::new(move |e: PointerEvent| {
            if e.pointer_type() != "mouse" {
                return;
            }
            if awaiting_exit.get() && entered_once.get() {
                awaiting_exit.set(false);
                entered_once.set(false);
                can_capture.set(true);
            }
        }) as Box<dyn FnMut(PointerEvent)>);
        let _ =
            canvas.add_event_listener_with_callback("pointerleave", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // --- pointerdown: click forces an immediate lock, ignoring cooldowns ---
    {
        let can_capture = can_capture.clone();
        let awaiting_exit = awaiting_exit.clone();
        let entered_once = entered_once.clone();
        let locked_at = locked_at.clone();
        let release_flag = release_flag.clone();
        let perf = performance.clone();
        let cb = Closure::wrap(Box::new(move |e: PointerEvent| {
            if e.pointer_type() != "mouse" {
                return;
            }
            // Bypass the release cooldown / re-entry state machine.
            release_flag.set(false);
            can_capture.set(true);
            awaiting_exit.set(false);
            entered_once.set(false);
            locked_at.set(perf.now());
            let _ = canvas.request_pointer_lock();
        }) as Box<dyn FnMut(PointerEvent)>);
        let _ =
            canvas.add_event_listener_with_callback("pointerdown", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // --- pointerlockchange: record lock time ---
    {
        let locked_at = locked_at.clone();
        let doc = document.clone();
        let perf = performance.clone();
        let cb = Closure::wrap(Box::new(move || {
            if doc.pointer_lock_element().is_some() {
                locked_at.set(perf.now());
            }
        }) as Box<dyn FnMut()>);
        let _ = document
            .add_event_listener_with_callback("pointerlockchange", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // --- mousemove: send deltas while pointer lock is active ---
    {
        let can_capture = can_capture.clone();
        let awaiting_exit = awaiting_exit.clone();
        let doc = document.clone();
        let perf = performance.clone();
        let pixel_ratio = window.device_pixel_ratio();
        let cb = Closure::wrap(Box::new(move |e: MouseEvent| {
            if doc.pointer_lock_element().is_none() {
                return;
            }
            // Ignore server-initiated release for 1 s after capture started.
            let since_lock = perf.now() - locked_at.get();
            if release_flag.get() && since_lock >= 1000.0 {
                release_flag.set(false);
                can_capture.set(false);
                awaiting_exit.set(true);
                doc.exit_pointer_lock();
                return;
            }
            // Discard stale release once cooldown expires naturally.
            if release_flag.get() {
                release_flag.set(false);
            }
            let dx = (Into::<f64>::into(e.movement_x()) * pixel_ratio) as i16;
            let dy = (Into::<f64>::into(e.movement_y()) * pixel_ratio) as i16;
            if dx == 0 && dy == 0 {
                return;
            }
            let msg = ClientDatagram::MouseMove { dx, dy };
            let bytes = msg.to_bytes();
            let buf = js_sys::Uint8Array::from(&bytes[..]);
            with_wt(|gwt| {
                let _ = gwt.writer.write_with_chunk(buf.as_ref());
            });
        }) as Box<dyn FnMut(MouseEvent)>);
        let _ = canvas.add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // --- mousedown / mouseup: send button events while locked ---
    {
        let doc = document.clone();
        let cb = Closure::wrap(Box::new(move |e: MouseEvent| {
            if doc.pointer_lock_element().is_none() {
                return;
            }
            e.prevent_default();
            let msg = ClientDatagram::MouseButton {
                button: e.button() as u8,
                pressed: true,
            };
            let bytes = msg.to_bytes();
            let buf = js_sys::Uint8Array::from(&bytes[..]);
            with_wt(|gwt| {
                let _ = gwt.writer.write_with_chunk(buf.as_ref());
            });
        }) as Box<dyn FnMut(MouseEvent)>);
        let _ = canvas.add_event_listener_with_callback("mousedown", cb.as_ref().unchecked_ref());
        cb.forget();
    }
    {
        let doc = document.clone();
        let cb = Closure::wrap(Box::new(move |e: MouseEvent| {
            if doc.pointer_lock_element().is_none() {
                return;
            }
            e.prevent_default();
            let msg = ClientDatagram::MouseButton {
                button: e.button() as u8,
                pressed: false,
            };
            let bytes = msg.to_bytes();
            let buf = js_sys::Uint8Array::from(&bytes[..]);
            with_wt(|gwt| {
                let _ = gwt.writer.write_with_chunk(buf.as_ref());
            });
        }) as Box<dyn FnMut(MouseEvent)>);
        let _ = canvas.add_event_listener_with_callback("mouseup", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // --- wheel: send scroll while locked ---
    {
        let doc = document.clone();
        let cb = Closure::wrap(Box::new(move |e: web_sys::WheelEvent| {
            if doc.pointer_lock_element().is_none() {
                return;
            }
            e.prevent_default();
            // EIS scroll_discrete expects values in 120-units (one wheel
            // notch). Browser deltaMode: 0 = pixels, 1 = lines, 2 = pages.
            let factor = match e.delta_mode() {
                0 => 1.0,
                1 => 40.0, // ~3 lines per notch
                _ => 120.0, // ~1 page per notch
            };
            let msg = ClientDatagram::Scroll {
                dx: (e.delta_x() * factor) as i32,
                dy: (e.delta_y() * factor) as i32,
            };
            let bytes = msg.to_bytes();
            let buf = js_sys::Uint8Array::from(&bytes[..]);
            with_wt(|gwt| {
                let _ = gwt.writer.write_with_chunk(buf.as_ref());
            });
        }) as Box<dyn FnMut(web_sys::WheelEvent)>);
        let _ = canvas.add_event_listener_with_callback("wheel", cb.as_ref().unchecked_ref());
        cb.forget();
    }
}
