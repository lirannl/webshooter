//! Client-side input throttle.
//!
//! Every input datagram flows through [`send_input`], which applies a minimum
//! spacing between sends and coalesces whatever arrives inside that spacing into
//! one flush. Coalescing keeps the newest state — mouse deltas are summed (so no
//! movement is lost), touch/scroll accumulate, gamepad snapshots are reduced to
//! the latest one — which preserves the total input at a fraction of the event
//! rate.
//!
//! Two reasons for the spacing, and they are not the same reason:
//!
//! - **The floor** ([`shared::throttle::INPUT_MIN_INTERVAL_MS`]) is applied
//!   whether or not the server has asked for anything, and is shared with the
//!   server so both ends enforce the same rate. Without it the browser's event
//!   rate *is* the wire rate, which is how a multitouch flood arrives at the
//!   compositor at thousands of events per second before any throttle message
//!   has been sent, let alone acted on.
//! - **The server's request** can make the spacing coarser, never finer.
//!
//! Ordinary input is unaffected by the floor: a finger dragging produces events
//! 16 ms apart, so the 4 ms floor is never reached and nothing waits.

use shared::client_datagram::{ClientDatagram, coalesce_input};
use shared::throttle::{INPUT_MIN_INTERVAL_MS, effective_interval_ms};
use std::cell::RefCell;
use std::collections::VecDeque;
use wasm_bindgen::prelude::*;

struct Throttle {
    /// Minimum spacing between input datagrams (ms), already resolved through
    /// the shared floor. Never zero: an unthrottled stream is the case that
    /// wastes bandwidth, so it must not mean "as fast as the browser fires".
    interval_ms: f64,
    /// `performance.now()` when the last input datagram was actually sent.
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
        interval_ms: INPUT_MIN_INTERVAL_MS as f64,
        last_send: 0.0,
        pending: VecDeque::new(),
        flush_id: None,
    }) };
    static FLUSH_CB: Closure<dyn FnMut()> =
        Closure::wrap(Box::new(flush_pending) as Box<dyn FnMut()>);
}

/// Apply the spacing the server requested, resolved through the shared floor.
///
/// `last_send` is deliberately not touched: if the request arrives long after the
/// last send, the elapsed time already satisfies the new spacing and the next
/// event goes straight out, rather than being made to wait a whole interval from
/// a moment the client did not choose. [`flush_pending`] re-arms its timer if the
/// interval grew while one was outstanding.
pub fn set_throttle(interval_ms: u16) {
    THROTTLE.with(|t| {
        t.borrow_mut().interval_ms = f64::from(effective_interval_ms(interval_ms));
    });
}

/// Send an input datagram, honouring the minimum spacing.
pub fn send_input(msg: ClientDatagram) {
    let now = now_ms();
    THROTTLE.with(|t| {
        let mut t = t.borrow_mut();
        if now - t.last_send >= t.interval_ms {
            // Spacing satisfied: write straight through.
            t.last_send = now;
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
        .set_timeout_with_callback_and_timeout_and_arguments_0(&flush_fn, delay.max(0.0) as i32)
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
