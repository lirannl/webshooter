//! Client-side input throttle.
//!
//! Every input datagram flows through [`send_input`], which applies a minimum
//! spacing between sends and coalesces whatever arrives inside that spacing into
//! one queue. Coalescing keeps the newest state — mouse deltas are summed (so no
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
//!
//! The queue drains **one datagram per spacing**, never all at once. A flush
//! that emptied it in one go would put an entire window's events on the wire in
//! a single burst — the cap arriving as the flood it is meant to prevent, at a
//! rate no spacing can describe — and the batch would be handed to the server's
//! own input budget as a lump it has to refuse part of. One per tick makes the
//! cap a rate: at most [`shared::throttle::INPUT_MIN_INTERVAL_MS`] between any
//! two datagrams, whichever of the two send paths produced them. The queue
//! itself is bounded at [`MAX_PENDING`], so a stream the cap cannot keep up
//! with costs memory at the cap's rate instead of at its own.

use shared::client_datagram::{ClientDatagram, coalesce_input};
use shared::throttle::{INPUT_MIN_INTERVAL_MS, effective_interval_ms};
use std::cell::RefCell;
use std::collections::VecDeque;
use wasm_bindgen::prelude::*;

/// Hard bound on queued input datagrams.
///
/// Coalescing is what keeps the queue honest for real input — a drag is one
/// entry however fast it fires, ten fingers are at most ten — so anything past
/// this is a stream the cap cannot keep up with, and the queue has to choose
/// what to lose. It always loses the *oldest*: a discarded press is a key that
/// never went down, whereas a discarded release is a key stuck down, and
/// evicting in arrival order guarantees the press is discarded before the
/// release that follows it.
///
/// The bound is also the worst-case age of a backlog: at one datagram per
/// [`INPUT_MIN_INTERVAL_MS`], a full queue drains in 128 ms.
const MAX_PENDING: usize = 32;

struct Throttle {
    /// Minimum spacing between input datagrams (ms), already resolved through
    /// the shared floor. Never zero: an unthrottled stream is the case that
    /// wastes bandwidth, so it must not mean "as fast as the browser fires".
    interval_ms: f64,
    /// `performance.now()` when the last input datagram was actually sent.
    /// Both send paths read it to decide whether they may send and write it
    /// when they do, which is why the spacing holds across them.
    last_send: f64,
    /// Input datagrams waiting for the next flush. Consecutive coalescable
    /// events are merged (mouse/scroll deltas summed, touch/gamepad reduced to
    /// the newest snapshot) so a busy window collapses to a handful of
    /// datagrams instead of hundreds, and the queue is bounded by
    /// [`MAX_PENDING`].
    pending: VecDeque<ClientDatagram>,
    /// Handle of the outstanding `setTimeout` that flushes `pending`.
    flush_id: Option<i32>,
}

impl Throttle {
    /// A fresh throttle at the floor interval, as a page that has just loaded
    /// sees it: nothing has been sent, so the spacing is measured from page
    /// load — and by the time the page can produce input at all, the elapsed
    /// time has already satisfied it.
    const fn new() -> Self {
        Self {
            interval_ms: INPUT_MIN_INTERVAL_MS as f64,
            last_send: 0.0,
            pending: VecDeque::new(),
            flush_id: None,
        }
    }

    /// Whether the spacing has elapsed as of `now` and the next datagram may go
    /// out. The write-straight-through path and the flush both gate on this and
    /// both record `last_send`, which is what makes "at most one datagram per
    /// spacing" a property of the throttle rather than of either caller.
    fn due(&self, now: f64) -> bool {
        now - self.last_send >= self.interval_ms
    }

    /// Queue an event for the next flush, merging it with what is already
    /// waiting wherever the two describe the same thing, and keep the queue
    /// within [`MAX_PENDING`] by discarding the oldest entries.
    fn enqueue(&mut self, msg: ClientDatagram) {
        coalesce_input(&mut self.pending, msg);
        while self.pending.len() > MAX_PENDING {
            self.pending.pop_front();
        }
    }

    /// Take the datagram due next, if the spacing has elapsed. Recording the
    /// send here is what holds the next one — flushed or straight through — a
    /// full spacing behind this one.
    fn take_due(&mut self, now: f64) -> Option<ClientDatagram> {
        if !self.due(now) {
            return None;
        }
        let msg = self.pending.pop_front()?;
        self.last_send = now;
        Some(msg)
    }
}

thread_local! {
    static THROTTLE: RefCell<Throttle> = const { RefCell::new(Throttle::new()) };
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
        if t.due(now) {
            // Spacing satisfied: write straight through.
            t.last_send = now;
            drop(t);
            send_raw(msg);
            return;
        }
        t.enqueue(msg);
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
/// [`schedule_flush`] (first arm after an event) and [`flush_pending`] (a
/// backlog still outstanding after a tick) compute their own delay; the
/// actual `setTimeout` bookkeeping is shared.
fn arm_flush(t: &mut Throttle, delay: f64) {
    let flush_fn = FLUSH_CB.with(|cb| cb.as_ref().unchecked_ref::<js_sys::Function>().clone());
    let handle = web_sys::window()
        .unwrap()
        .set_timeout_with_callback_and_timeout_and_arguments_0(&flush_fn, delay.max(0.0) as i32)
        .unwrap();
    t.flush_id = Some(handle);
}

/// Timer callback: send the *next* queued datagram, then re-arm for the one
/// after it.
///
/// One per tick, never the whole queue: see the module docs. Re-arming after
/// every tick is also what carries an interval that grew while the timer was
/// outstanding — the new due time is computed from `last_send`, so an
/// outstanding timer that fires early simply finds the spacing not yet elapsed
/// and waits the remainder.
fn flush_pending() {
    let now = now_ms();
    let msg = THROTTLE.with(|t| {
        let mut t = t.borrow_mut();
        t.flush_id = None;
        t.take_due(now)
    });
    if let Some(msg) = msg {
        send_raw(msg);
    }
    // Re-arm if a backlog remains (and only then: an empty queue arms nothing).
    schedule_flush();
}

fn now_ms() -> f64 {
    web_sys::window().unwrap().performance().unwrap().now()
}

fn send_raw(msg: ClientDatagram) {
    crate::send_datagram(msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clock resolution for [`advance`]: well under the spacing, so a flush
    /// that becomes due mid-window is noticed promptly rather than skipped
    /// over by the next sample.
    const STEP: f64 = 0.25;

    /// An event that coalescing never merges with another, so a stream of them
    /// is the worst case for both the queue and the cap.
    fn unmergeable(index: u8) -> ClientDatagram {
        ClientDatagram::TouchscreenRelease { index }
    }

    /// Run the clock from `now` to `until` in small steps, flushing whenever
    /// the throttle says a flush is due, and return how many datagrams reached
    /// the wire. This is the half of the throttle the timer callback drives;
    /// `Throttle::send` below is the half an event drives.
    fn advance(t: &mut Throttle, now: &mut f64, until: f64) -> usize {
        let mut sent = 0;
        while *now < until {
            *now = (*now + STEP).min(until);
            if t.take_due(*now).is_some() {
                sent += 1;
            }
        }
        sent
    }

    impl Throttle {
        /// The send path an arriving event takes, without the JS around it:
        /// straight through when the spacing is satisfied, queued otherwise.
        fn send(&mut self, msg: ClientDatagram, now: f64) -> bool {
            if self.due(now) {
                self.last_send = now;
                return true;
            }
            self.enqueue(msg);
            false
        }
    }

    /// The property the whole throttle exists for, stated as the flood that
    /// produced it: twice the cap in events per second, none of them
    /// coalescable, for a simulated second — and the wire never sees more than
    /// the cap allows.
    #[test]
    fn a_flood_is_capped_at_the_shared_rate() {
        let interval = f64::from(INPUT_MIN_INTERVAL_MS);
        let mut t = Throttle::new();
        let mut now = 0.0;
        let mut sent = 0;
        // 2000 events/s: twice the cap, and none of them waiting for the
        // spacing to elapse — each one arrives while the previous is still
        // queued behind it.
        let gap = interval / 2.0;
        let mut next_event = 0.0;
        for index in 0..2000 {
            sent += advance(&mut t, &mut now, next_event);
            if t.send(unmergeable((index % 256) as u8), now) {
                sent += 1;
            }
            next_event += gap;
        }
        // Drain what the last event left queued.
        sent += advance(&mut t, &mut now, next_event);

        let ceiling = (now / interval).floor() as usize + 1;
        assert!(
            sent <= ceiling,
            "a second of flooding sent {sent} datagrams, above the {ceiling} the \
             {interval} ms floor permits"
        );
        assert!(
            t.pending.len() <= MAX_PENDING,
            "the queue grew to {} entries, past the {MAX_PENDING} bound",
            t.pending.len()
        );
    }

    /// What fills the queue when nothing coalesces: the *oldest* entries go,
    /// because a discarded press is a key that never went down while a
    /// discarded release is a key stuck down.
    #[test]
    fn the_queue_is_bounded_and_drops_its_oldest_entries() {
        let mut t = Throttle::new();
        for index in 0..(MAX_PENDING + 8) {
            // Everything lands inside one spacing window — four times as many
            // events as fit — so the queue is the only thing holding them.
            t.enqueue(unmergeable(index as u8));
        }
        assert_eq!(
            t.pending.len(),
            MAX_PENDING,
            "the queue must not grow past its bound"
        );
        assert_eq!(
            t.pending.front(),
            Some(&unmergeable(8)),
            "the oldest events are the ones discarded"
        );
        assert_eq!(
            t.pending.back(),
            Some(&unmergeable((MAX_PENDING + 7) as u8)),
            "the newest event is the one kept"
        );
    }

    /// Real input must not be made to wait for the flood policy: at ordinary
    /// event rates every datagram is already due and goes straight out, with
    /// nothing queued behind it.
    #[test]
    fn input_slower_than_the_floor_is_never_delayed() {
        let interval = f64::from(INPUT_MIN_INTERVAL_MS);
        let mut t = Throttle::new();
        let mut now = 0.0;
        let mut sent = 0;
        for _ in 0..100 {
            // A 60 Hz device, plus the 16 ms that a finger drag produces.
            now += interval * 4.0;
            if t.send(ClientDatagram::MouseMove { dx: 1, dy: 1 }, now) {
                sent += 1;
            }
        }
        assert_eq!(sent, 100, "every datagram must have gone straight through");
        assert!(t.pending.is_empty(), "nothing may be left waiting");
    }

    /// Coalescing is what stops ordinary input from *needing* the queue: a
    /// drag inside one window arrives as a single datagram carrying the whole
    /// movement, so the cap costs the user their delta rather than their rate.
    #[test]
    fn a_drag_inside_one_window_is_one_datagram_with_the_total_delta() {
        let mut t = Throttle::new();
        for dx in 1..=3i16 {
            // All three inside the first spacing window, so nothing is due yet
            // and the queue is where they meet.
            t.send(
                ClientDatagram::MouseMove { dx, dy: -dx },
                f64::from(dx) / 2.0,
            );
        }
        assert_eq!(t.pending.len(), 1, "a drag must not build a backlog");
        let msg = t.take_due(4.0).expect("the spacing has elapsed by 4 ms");
        assert_eq!(
            msg,
            ClientDatagram::MouseMove { dx: 6, dy: -6 },
            "the whole movement must survive, not the last of it"
        );
        assert!(
            t.take_due(8.0).is_none(),
            "and only one datagram may carry it"
        );
    }

    /// The straight-through path and the flush share one spacing: two sends
    /// may never land closer together than the floor, whichever path took them.
    #[test]
    fn no_two_datagrams_are_ever_closer_than_the_spacing() {
        let interval = f64::from(INPUT_MIN_INTERVAL_MS);
        let mut t = Throttle::new();
        let mut now = 0.0;
        let mut last_send: Option<f64> = None;
        let check = |sent_at: f64, last: &mut Option<f64>| {
            if let Some(previous) = *last {
                assert!(
                    sent_at - previous >= interval - STEP,
                    "datagrams {sent_at} and {previous} are {:.2} ms apart, inside the \
                     {interval} ms spacing",
                    sent_at - previous
                );
            }
            *last = Some(sent_at);
        };
        let mut next_event = 0.0;
        for index in 0..400 {
            while now < next_event {
                now = (now + STEP).min(next_event);
                if t.take_due(now).is_some() {
                    check(now, &mut last_send);
                }
            }
            if t.due(now) {
                t.last_send = now;
                check(now, &mut last_send);
            } else {
                t.enqueue(unmergeable((index % 256) as u8));
            }
            next_event += interval / 3.0;
        }
        while !t.pending.is_empty() {
            now += STEP;
            if t.take_due(now).is_some() {
                check(now, &mut last_send);
            }
        }
        assert!(last_send.is_some(), "the scenario must actually have sent something");
    }
}
