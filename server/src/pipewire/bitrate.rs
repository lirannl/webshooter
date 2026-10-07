//! Adaptive video bitrate.
//!
//! The configured bitrate is a **ceiling**, not a target. On a link that cannot
//! carry it the encoder is the only party that can do anything about it: the
//! frames are already produced, and the queue in front of the transport is
//! somewhere we cannot reach from here. Asking the encoder for fewer bits per
//! second is the one lever that acts before the frames exist.
//!
//! Two properties matter more than the exact numbers:
//!
//! - **It only ever goes down from the ceiling.** A session that was configured
//!   for a fast link and then moved to a slow one must not find itself encoding
//!   above what it was always allowed to send, so the configured value bounds
//!   the policy from above no matter how healthy the link looks.
//! - **It falls faster than it rises.** Congestion is evidence; its absence is
//!   not. A link that is merely quiet is not a link with spare capacity, so
//!   recovery takes consecutive clean samples and is capped at the ceiling, while
//!   a single congested sample halves the bitrate at once.
//!
//! The signal is the QUIC path's own accounting — lost packets, black holes and
//! congestion events — sampled where the link is already being sampled for
//! reporting. What this module decides is only what that signal means for the
//! encoder.

/// The bitrate is halved/doubled rather than moved to a fixed ladder, so the
/// policy works for any configured ceiling instead of only for the one the
/// ladder was written against.
const STEP_DIVISOR: u32 = 2;

/// The floor, in kbit/s.
///
/// Below this a 1080p picture stops being a picture. A session that genuinely
/// cannot be carried is better off being coarse than being a slideshow of
/// dropped keyframes, but it should not be pushed all the way to nothing.
pub(crate) const MIN_KBPS: u32 = 250;

/// What the last sample of the path said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkPressure {
    /// Packets were lost, black-holed, or the congestion controller reacted.
    Congested,
    /// None of that since the previous sample.
    Clear,
}

/// The bitrate to encode at next, given what the path just did.
///
/// `ceiling_kbps` is the configured value and is never exceeded. `current` is
/// clamped into range rather than trusted, so a bad starting value (a ceiling
/// lowered by reconfiguration, say) cannot strand the encoder above its ceiling.
pub(crate) fn next_bitrate(current: u32, ceiling_kbps: u32, pressure: LinkPressure) -> u32 {
    let ceiling = ceiling_kbps.max(1);
    let current = current.clamp(1, ceiling);

    let next = match pressure {
        LinkPressure::Congested => {
            let halved = current / STEP_DIVISOR;
            // The floor stops the halving, but it must never *raise* a bitrate
            // that is already below it. Clamping upwards would make congestion
            // silently inert for any ceiling configured under the floor: a
            // session set to 120 kbit/s would stay at 120 no matter how badly
            // the path was congested, which is the one case the policy exists
            // for.
            if current >= MIN_KBPS {
                halved.max(MIN_KBPS)
            } else {
                halved.max(1)
            }
        }
        LinkPressure::Clear => current.saturating_mul(STEP_DIVISOR).min(ceiling),
    };
    next.clamp(1, ceiling)
}

#[cfg(test)]
mod tests {
    use super::{LinkPressure, MIN_KBPS, next_bitrate};

    const CEILING: u32 = 4000;

    /// Congestion halves at once, every sample, down to the floor — and then
    /// stops, rather than halving a floor of 250 into 125 and 62.
    #[test]
    fn congestion_halves_geometrically_to_the_floor() {
        let mut bitrate = CEILING;
        let mut seen = vec![bitrate];
        for _ in 0..10 {
            bitrate = next_bitrate(bitrate, CEILING, LinkPressure::Congested);
            seen.push(bitrate);
        }
        assert_eq!(
            seen,
            vec![4000, 2000, 1000, 500, 250, 250, 250, 250, 250, 250, 250]
        );
    }

    /// A clean path walks back up, one doubling per sample, and stops exactly at
    /// the ceiling rather than overshooting it.
    #[test]
    fn recovery_doubles_back_to_the_ceiling_and_stops() {
        let mut bitrate = MIN_KBPS;
        let mut seen = vec![bitrate];
        for _ in 0..5 {
            bitrate = next_bitrate(bitrate, CEILING, LinkPressure::Clear);
            seen.push(bitrate);
        }
        assert_eq!(seen, vec![250, 500, 1000, 2000, 4000, 4000]);
    }

    /// The ceiling is the property the operator configured; nothing the link
    /// reports may push the encoder past it, in either direction.
    #[test]
    fn the_ceiling_is_never_exceeded() {
        for current in [0, 1, 250, 1999, 4000, 4001, 999_999] {
            for pressure in [LinkPressure::Clear, LinkPressure::Congested] {
                assert!(
                    next_bitrate(current, CEILING, pressure) <= CEILING,
                    "current {current} under {pressure:?} exceeded the ceiling"
                );
            }
        }
    }

    /// And it is never undercut either: a bitrate at or above the floor never
    /// falls below it.
    #[test]
    fn congestion_never_drops_below_the_floor() {
        for current in [MIN_KBPS, 251, 1000, 4000] {
            assert!(
                next_bitrate(current, CEILING, LinkPressure::Congested) >= MIN_KBPS,
                "current {current} fell through the floor"
            );
        }
    }

    /// A ceiling configured below the floor is obeyed rather than "corrected" up
    /// to the floor — and, crucially, it is still reducible: congestion must not
    /// be silently inert just because the operator configured a small number.
    #[test]
    fn a_ceiling_below_the_floor_is_honoured_and_still_reducible() {
        assert_eq!(next_bitrate(120, 120, LinkPressure::Congested), 60);
        assert_eq!(next_bitrate(60, 120, LinkPressure::Congested), 30);
        // Recovery stops at the configured ceiling, not the floor.
        assert_eq!(next_bitrate(30, 120, LinkPressure::Clear), 60);
        assert_eq!(next_bitrate(60, 120, LinkPressure::Clear), 120);
        assert_eq!(next_bitrate(120, 120, LinkPressure::Clear), 120);
    }

    /// The same rule without a sub-floor ceiling: a bitrate already under the
    /// floor keeps halving rather than being lifted back up to it.
    #[test]
    fn a_current_below_the_floor_still_halves() {
        assert_eq!(next_bitrate(120, CEILING, LinkPressure::Congested), 60);
    }

    /// A stale value from before a reconfiguration is pulled back under the new
    /// ceiling rather than being trusted and halved from above it.
    #[test]
    fn a_current_value_above_the_ceiling_is_pulled_back() {
        assert_eq!(next_bitrate(8000, CEILING, LinkPressure::Clear), CEILING);
        assert_eq!(next_bitrate(8000, CEILING, LinkPressure::Congested), 2000);
    }

    /// Already at the bottom, still congested: no change, so the encoder is not
    /// woken to be told nothing.
    #[test]
    fn being_at_the_floor_is_stable() {
        assert_eq!(
            next_bitrate(MIN_KBPS, CEILING, LinkPressure::Congested),
            MIN_KBPS
        );
        assert_eq!(next_bitrate(CEILING, CEILING, LinkPressure::Clear), CEILING);
    }

    /// A full down-then-up cycle returns to where it started, so a link that
    /// merely wobbles settles instead of drifting.
    #[test]
    fn a_wobbling_link_settles_back_at_the_ceiling() {
        let mut bitrate = CEILING;
        for _ in 0..3 {
            bitrate = next_bitrate(bitrate, CEILING, LinkPressure::Congested);
        }
        for _ in 0..10 {
            bitrate = next_bitrate(bitrate, CEILING, LinkPressure::Clear);
        }
        assert_eq!(bitrate, CEILING);
    }
}
