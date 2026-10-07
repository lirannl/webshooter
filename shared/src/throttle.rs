//! Input rate limiting, shared by both ends.
//!
//! The server throttles because it cannot afford the event rate; the client
//! throttles because sending faster than the server can accept wastes bandwidth
//! on both sides and, on a congested link, spends the capacity the *video* needs.
//! Both reasons come from the same number, so both ends derive it from here
//! rather than each holding a copy.
//!
//! What this module deliberately does **not** do is measure anything. The
//! server's own queue depth is measurable and its throttle monitor watches that;
//! how fast a given compositor can absorb input is not measurable from either
//! process, and varies by orders of magnitude between a desktop and a phone-class
//! compositor under load. So the floor is a constant — the lowest common
//! denominator both ends can live with — and only the *server's* request can make
//! the spacing coarser.

/// Floor on the spacing between consecutive input events: 4 ms, so **at most 250
/// events a second**.
///
/// The most conservative value that cannot be noticed. A single finger dragging
/// produces 60-120 events a second, so ordinary input never reaches this and pays
/// no added latency; everything above it — extra fingers, a flooding client, a
/// compositor with no headroom — is what the cap exists to absorb.
pub const INPUT_MIN_INTERVAL_MS: u16 = 4;

/// The spacing actually enforced for a server-requested interval.
///
/// `0` means "no throttling requested", which is the healthy case and the default
/// before the first sample. It resolves to the floor rather than to zero: an
/// unthrottled client is exactly the case that wastes bandwidth, so it must not
/// mean "send at whatever rate the browser generates".
///
/// One definition, so the two ends cannot disagree about the spacing they are
/// each applying to the same stream.
pub fn effective_interval_ms(requested_ms: u16) -> u16 {
    requested_ms.max(INPUT_MIN_INTERVAL_MS)
}

/// The spacing to hold after forwarding `events` input events at
/// `requested_ms`.
///
/// **Per event, not per batch.** A batch is how many events happened to arrive
/// together, so charging one interval for a whole batch lets a burst through at
/// `events` times the intended rate. With ten fingers coalescing to ten events
/// per window, that is ten times the cap — which is how a multitouch flood
/// reached the compositor at 10,000 events/s while every log line looked correct.
///
/// At least one interval, so an empty batch cannot open the gate completely.
pub fn spacing_ms_after_events(events: usize, requested_ms: u16) -> u64 {
    u64::from(effective_interval_ms(requested_ms)).saturating_mul(events.max(1) as u64)
}

#[cfg(test)]
mod tests {
    use super::{INPUT_MIN_INTERVAL_MS, effective_interval_ms, spacing_ms_after_events};

    /// The documented cap and the constant that implements it must not drift
    /// apart, because every other number here is derived from it.
    #[test]
    fn the_floor_is_four_milliseconds() {
        assert_eq!(INPUT_MIN_INTERVAL_MS, 4);
        assert_eq!(1000.0 / f64::from(INPUT_MIN_INTERVAL_MS), 250.0);
    }

    /// A server asking for nothing gets the floor, not zero — the healthy case
    /// must still be capped, or the waste happens exactly when nobody is asking
    /// for it.
    #[test]
    fn an_unthrottled_request_still_gets_the_floor() {
        assert_eq!(effective_interval_ms(0), INPUT_MIN_INTERVAL_MS);
    }

    /// A server asking for something coarser is obeyed exactly.
    #[test]
    fn a_coarser_request_is_never_raised_to_the_floor() {
        for requested in [4u16, 8, 16, 32, 64, 128, 1000] {
            assert_eq!(effective_interval_ms(requested), requested);
        }
    }

    /// Nothing can make the spacing finer than the floor, from any input.
    #[test]
    fn the_floor_is_a_floor() {
        for requested in [0u16, 1, 2, 3, 4] {
            assert!(effective_interval_ms(requested) >= INPUT_MIN_INTERVAL_MS);
        }
    }

    /// The property the throttle exists to enforce: whatever the batch size, the
    /// event rate never exceeds the cap.
    #[test]
    fn the_event_rate_never_exceeds_the_cap() {
        let intended = 1000.0 / f64::from(INPUT_MIN_INTERVAL_MS);
        for events in [1usize, 2, 5, 10, 64] {
            for requested in [0u16, 8, 128] {
                let ms = spacing_ms_after_events(events, requested);
                let rate = events as f64 / (ms as f64 / 1000.0);
                assert!(
                    rate <= intended + 1.0,
                    "{events} events in {ms}ms ran at {rate:.0}/s, above {intended:.0}/s"
                );
            }
        }
    }

    /// Ten fingers, stated as the number a person would recognise: 10 events in a
    /// 40 ms window is 250/s, not 10,000/s.
    #[test]
    fn ten_fingers_land_at_the_cap() {
        assert_eq!(spacing_ms_after_events(10, 0), 40);
        assert_eq!(10.0 / (40.0 / 1000.0), 250.0);
    }

    /// One event is unchanged: the common case must not be slowed by a
    /// correction meant for bursts.
    #[test]
    fn one_event_waits_exactly_one_interval() {
        for requested in [0u16, 8, 32] {
            assert_eq!(
                spacing_ms_after_events(1, requested),
                u64::from(effective_interval_ms(requested))
            );
        }
    }

    /// An empty batch still closes the gate for one interval; zero would open it
    /// completely and let the next burst straight through.
    #[test]
    fn an_empty_batch_still_waits_one_interval() {
        assert_eq!(
            spacing_ms_after_events(0, 0),
            u64::from(INPUT_MIN_INTERVAL_MS)
        );
        assert!(spacing_ms_after_events(0, 32) > 0);
    }

    /// The scaling must hold at every spacing the server can ask for, or the
    /// heaviest case is the one that leaks.
    #[test]
    fn scaling_holds_at_every_requested_interval() {
        for requested_ms in [4u16, 8, 16, 32, 64, 128] {
            assert_eq!(
                spacing_ms_after_events(10, requested_ms),
                u64::from(requested_ms) * 10,
                "at {requested_ms}ms"
            );
        }
    }
}
