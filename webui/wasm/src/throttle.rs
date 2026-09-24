//! Client-side input throttle.
//!
//! The server asks us to slow down when its input-processing pipeline
//! approaches saturation by sending `ServerDatagram::Throttle { interval_ms }`.
//! Every input datagram flows through [`send_input`]: while throttled, events
//! are coalesced into a small queue and flushed at the requested minimum
//! spacing instead of being written to the WebTransport stream once per
//! browser event. Coalescing keeps the newest state — mouse deltas are summed
//! (so no movement is lost), touch/scroll accumulate, gamepad snapshots are
//! reduced to the latest one — which preserves the total input at a fraction
//! of the event rate.

use shared::client_datagram::{ClientDatagram, coalesce_input};
use std::cell::RefCell;
use std::collections::VecDeque;
use wasm_bindgen::prelude::*;

struct Throttle {
    /// Server-requested minimum spacing between input datagrams (ms).
    /// 0 = no throttling.
    interval_ms: f64,
    /// `performance.now()` when the last input batch was actually sent.
    last_send: f64,
    /// Input datagrams waiting for the next flush. Consecutive coalescable
    /// events are merged (mouse/scroll deltas summed, touch/gamepad reduced to
    /// the newest snapshot) so a busy window collapses to a handful of
    /// datagrams instead of hundreds.
    pending: VecDeque<ClientDatagram>,
    /// Handle of the outstanding `setTimeout` that flushes `pending`.
    flush_id: Option<i32>,
}

thread_local! {
    static THROTTLE: RefCell<Throttle> = const { RefCell::new(Throttle {
        interval_ms: 0.0,
        last_send: 0.0,
        pending: VecDeque::new(),
        flush_id: None,
    }) };
    static FLUSH_CB: Closure<dyn FnMut()> =
        Closure::wrap(Box::new(flush_pending) as Box<dyn FnMut()>);
}

/// Apply the spacing the server requested. `0` lifts throttling and drains any
/// queued input immediately so there is no artificial lag left behind.
pub fn set_throttle(interval_ms: u16) {
    let drain = THROTTLE.with(|t| {
        let mut t = t.borrow_mut();
        let was_on = t.interval_ms > 0.0;
        t.interval_ms = interval_ms as f64;
        if t.interval_ms <= 0.0 {
            // Throttling lifted: cancel the pending flush and send whatever is
            // queued now.
            t.flush_id = None;
            Some(Vec::from(std::mem::take(&mut t.pending)))
        } else if !was_on {
            // Just switched on: base the first spacing on this moment so we
            // don't immediately release a burst.
            t.last_send = now_ms();
            None
        } else {
            None
        }
    });
    if let Some(msgs) = drain {
        for msg in msgs {
            send_raw(&msg);
        }
    }
}

/// Send an input datagram, honouring the server's throttle.
pub fn send_input(msg: ClientDatagram) {
    THROTTLE.with(|t| {
        let mut t = t.borrow_mut();
        if t.interval_ms <= 0.0 {
            // Not throttled: write straight through, zero added latency.
            t.last_send = now_ms();
            drop(t);
            send_raw(&msg);
            return;
        }
        coalesce_input(&mut t.pending, msg);
    });
    schedule_flush();
}

/// Arm a timer that flushes the queue once the server-requested spacing since
/// the last send has elapsed. A single outstanding timer is enough: events
/// arriving meanwhile coalesce into the same flush.
fn schedule_flush() {
    THROTTLE.with(|t| {
        let mut t = t.borrow_mut();
        if t.flush_id.is_some() || t.pending.is_empty() {
            return;
        }
        let due = t.last_send + t.interval_ms;
        arm_flush(&mut t, due - now_ms());
    });
}

/// Arm the flush timer for a delay (ms), recording it on `t`. Both
/// [`schedule_flush`] (first arm after an event) and [`flush_pending`] (the
/// interval grew while the timer was armed) compute their own delay; the
/// actual `setTimeout` bookkeeping is shared.
fn arm_flush(t: &mut Throttle, delay: f64) {
    let flush_fn = FLUSH_CB.with(|cb| cb.as_ref().unchecked_ref::<js_sys::Function>().clone());
    let handle = web_sys::window()
        .unwrap()
        .set_timeout_with_callback_and_timeout_and_arguments_0(
            &flush_fn,
            delay.max(0.0) as i32,
        )
        .unwrap();
    t.flush_id = Some(handle);
}

/// Timer callback: send everything queued since the last flush, observing the
/// server-requested spacing.
fn flush_pending() {
    let now = now_ms();
    THROTTLE.with(|t| {
        let mut t = t.borrow_mut();
        t.flush_id = None;
        if t.pending.is_empty() {
            return;
        }
        let due = t.last_send + t.interval_ms;
        if now < due {
            // The interval grew while the timer was armed: wait the remainder.
            arm_flush(&mut t, due - now);
            return;
        }
        let msgs = std::mem::take(&mut t.pending);
        t.last_send = now;
        drop(t);
        for msg in msgs {
            send_raw(&msg);
        }
    });
}

fn now_ms() -> f64 {
    web_sys::window().unwrap().performance().unwrap().now()
}

fn send_raw(msg: &ClientDatagram) {
    crate::send_datagram(msg.clone());
}