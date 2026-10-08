//! One coalescing buffer per session for the input a client sends, drained at a
//! rate the compositor can absorb.
//!
//! The compositor — and the EIS devices every session injects through — is one
//! machine-wide resource, so the drain rate is a single server-wide budget, not
//! one per session: a per-session budget would be `sessions ×` the rate at the
//! compositor, which is exactly the saturation this exists to prevent. The rate
//! is a constant for the reason `shared::throttle` gives: how fast a given
//! compositor absorbs input is not measurable from either process.
//!
//! The buffer is keyed by `(variant, identity)` and insertion-ordered. A key
//! that is touched again keeps its position — FIFO by *first* touch — and a new
//! key goes to the back. Within a drain window a repeated key is superseded
//! (deltas add, state takes the newest value), so the buffer is bounded by the
//! number of distinct identities rather than by the event count. Nothing is
//! dropped on the floor: an event past the drain budget is deferred to the next
//! tick, never discarded, so a held key or a slow fingerprint still reaches the
//! compositor.

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use indexmap::IndexMap;

use shared::client_datagram::ClientDatagram;
use shared::throttle::INPUT_MIN_INTERVAL_MS;

use super::eis_keyboard::js_keycode_to_eis_key;

/// The identity a datagram coalesces under. Two datagrams with the same key
/// supersede one another; different keys each keep their own position.
#[derive(PartialEq, Eq, Hash, Debug)]
enum InputKey {
    MouseMove,
    Scroll,
    MouseButton(u8),
    /// The resolved Linux keycode, or `0` for a modifier or an unmapped code.
    ///
    /// Resolved rather than keyed on the raw string on purpose: the client
    /// chooses that string, so one key per string would let it allocate without
    /// limit. Two codes that resolve to the same key *are* the same physical
    /// key, so coalescing them is also the correct collapse.
    Keyboard(u32),
    Touchscreen(u8),
    TouchscreenRelease(u8),
    Gamepad(u8),
    GamepadDisconnect(u8),
}

impl InputKey {
    fn of(msg: &ClientDatagram) -> Option<Self> {
        Some(match msg {
            ClientDatagram::MouseMove { .. } => Self::MouseMove,
            ClientDatagram::Scroll { .. } => Self::Scroll,
            ClientDatagram::MouseButton { button, .. } => Self::MouseButton(*button),
            ClientDatagram::Keyboard { keycode, .. } => {
                Self::Keyboard(js_keycode_to_eis_key(keycode).unwrap_or(0))
            }
            ClientDatagram::Touchscreen { index, .. } => Self::Touchscreen(*index),
            ClientDatagram::TouchscreenRelease { index } => Self::TouchscreenRelease(*index),
            ClientDatagram::Gamepad { id, .. } => Self::Gamepad(*id),
            ClientDatagram::GamepadDisconnect { id } => Self::GamepadDisconnect(*id),
            _ => return None,
        })
    }

    /// The continuous entry a lifecycle end terminates.
    ///
    /// A release or a disconnect is keyed by the same identity as the state it
    /// ends, and removes it. That is what makes a restart a *fresh* key at the
    /// back instead of an in-place update: without it, `down, release, down`
    /// would leave the second `down` sitting at the first one's position, and a
    /// drain could emit the release after the re-press — ending with the finger
    /// up while it is physically down. Removing the entry forces the new press
    /// behind the release, so order reconstructs as down, release, down.
    ///
    /// A press deliberately does not terminate a pending release: that would
    /// reorder a real `down, release, down` into `down, down` and lose the lift.
    fn terminates(&self) -> Option<Self> {
        match self {
            Self::TouchscreenRelease(index) => Some(Self::Touchscreen(*index)),
            Self::GamepadDisconnect(id) => Some(Self::Gamepad(*id)),
            _ => None,
        }
    }
}

/// The per-session coalescing buffer. Plain data: it is driven from the task
/// that already converts client datagrams into EIS events, so no lock is needed.
#[derive(Default)]
pub struct InputCoalesce {
    pending: IndexMap<InputKey, ClientDatagram>,
}

impl InputCoalesce {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one input datagram in.
    pub fn push(&mut self, msg: ClientDatagram) {
        let Some(key) = InputKey::of(&msg) else {
            return;
        };
        if let Some(ended) = key.terminates() {
            self.pending.shift_remove(&ended);
        }
        match self.pending.get_mut(&key) {
            Some(existing) => merge(existing, msg),
            None => {
                self.pending.insert(key, msg);
            }
        }
    }

    /// Drain from the front, in FIFO order, up to whatever the shared budget
    /// grants, leaving the rest pending for the next tick.
    pub fn drain_budgeted(&mut self) -> Vec<ClientDatagram> {
        let grant = take_input_budget(self.pending.len());
        self.drain_front(grant)
    }

    /// Drain at most `max` entries from the front. The unit tests drive this
    /// directly so they do not depend on the process-wide budget's phase.
    pub(crate) fn drain_front(&mut self, max: usize) -> Vec<ClientDatagram> {
        if max == 0 {
            return Vec::new();
        }
        let end = max.min(self.pending.len());
        self.pending.drain(0..end).map(|(_, msg)| msg).collect()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.pending.len()
    }
}

/// The same key was seen twice within a drain window. Mirror the per-variant
/// rules of [`shared::client_datagram::coalesce_input`]: delta variants add,
/// everything else takes the newest. Here the key has already established that
/// the variant and identity match, which is what collapses that function's
/// scan to these two cases.
fn merge(pending: &mut ClientDatagram, incoming: ClientDatagram) {
    match pending {
        ClientDatagram::MouseMove { dx, dy } => {
            if let ClientDatagram::MouseMove { dx: nx, dy: ny } = &incoming {
                *dx = dx.saturating_add(*nx);
                *dy = dy.saturating_add(*ny);
                return;
            }
        }
        ClientDatagram::Scroll { dx, dy } => {
            if let ClientDatagram::Scroll { dx: nx, dy: ny } = &incoming {
                *dx = dx.saturating_add(*nx);
                *dy = dy.saturating_add(*ny);
                return;
            }
        }
        _ => {}
    }
    *pending = incoming;
}

/// Events per second the compositor may be handed, server-wide.
const INPUT_RATE_PER_SEC: f64 = 1000.0 / INPUT_MIN_INTERVAL_MS as f64;

/// Instantaneous allowance, so a genuine micro-burst is not smeared into
/// latency. Small: sustained throughput is the rate, and the burst only absorbs
/// the jitter between ticks.
const INPUT_BURST: f64 = 32.0;

/// The coalesce window: how long a datagram may wait to be folded before a
/// drain. Equal to the floor interval, so folding never adds more delay than the
/// throttle already allows between events.
pub const INPUT_DRAIN_INTERVAL: Duration = Duration::from_millis(INPUT_MIN_INTERVAL_MS as u64);

struct Budget {
    tokens: f64,
    last: Instant,
}

impl Budget {
    /// Refill from the time since the last call, then spend up to `want`.
    fn grant(&mut self, want: usize, now: Instant) -> usize {
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * INPUT_RATE_PER_SEC).min(INPUT_BURST);
        let grant = (self.tokens.floor() as usize).min(want);
        self.tokens -= grant as f64;
        grant
    }
}

static INPUT_BUDGET: LazyLock<Mutex<Budget>> = LazyLock::new(|| {
    Mutex::new(Budget {
        tokens: INPUT_BURST,
        last: Instant::now(),
    })
});

/// Spend up to `want` tokens of the server-wide input budget, refilling lazily
/// from elapsed time, and report how many were granted.
fn take_input_budget(want: usize) -> usize {
    if want == 0 {
        return 0;
    }
    INPUT_BUDGET.lock().unwrap().grant(want, Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::client_datagram::Modifiers;

    fn key(code: &str) -> ClientDatagram {
        ClientDatagram::Keyboard {
            keycode: code.into(),
            modifiers: Modifiers::empty(),
        }
    }

    fn touch(index: u8, x: u16, y: u16) -> ClientDatagram {
        ClientDatagram::Touchscreen { index, x, y }
    }

    fn release(index: u8) -> ClientDatagram {
        ClientDatagram::TouchscreenRelease { index }
    }

    fn gamepad(id: u8, buttons: u32) -> ClientDatagram {
        ClientDatagram::Gamepad {
            id,
            buttons,
            lx: 0,
            ly: 0,
            rx: 0,
            ry: 0,
            lt: 0,
            rt: 0,
            motion: None,
        }
    }

    /// A key that is touched again keeps its position, so the drain order is by
    /// first touch and not by latest update.
    #[test]
    fn a_repeated_key_stays_put() {
        let mut c = InputCoalesce::new();
        c.push(ClientDatagram::MouseMove { dx: 1, dy: 1 });
        c.push(key("KeyA"));
        c.push(key("KeyB"));
        c.push(ClientDatagram::MouseMove { dx: 2, dy: 3 });

        assert_eq!(c.len(), 3);
        assert_eq!(
            c.drain_front(3),
            vec![
                ClientDatagram::MouseMove { dx: 3, dy: 4 },
                key("KeyA"),
                key("KeyB"),
            ],
            "the move must fold in place and stay ahead of the keys"
        );
    }

    /// The lifecycle case that motivates keying the release: a lift must land
    /// between the two presses, and the entry that precedes it is removed rather
    /// than left to be emitted after the re-press.
    #[test]
    fn a_release_terminates_the_pending_press() {
        let mut c = InputCoalesce::new();
        c.push(touch(0, 10, 10));
        c.push(release(0));
        c.push(touch(0, 20, 20));

        assert_eq!(
            c.drain_front(10),
            vec![release(0), touch(0, 20, 20)],
            "the pre-release press must be gone, and the re-press must follow the lift"
        );
    }

    /// A controller that reconnects after a disconnect is a fresh key too, so
    /// the pre-disconnect snapshot cannot reappear in front of the disconnect.
    #[test]
    fn a_disconnect_terminates_the_pending_snapshot() {
        let mut c = InputCoalesce::new();
        c.push(gamepad(1, 0b01));
        c.push(ClientDatagram::GamepadDisconnect { id: 1 });
        c.push(gamepad(1, 0b10));

        let out = c.drain_front(10);
        assert!(matches!(
            out.as_slice(),
            [
                ClientDatagram::GamepadDisconnect { id: 1 },
                ClientDatagram::Gamepad { buttons: 0b10, .. }
            ]
        ));
    }

    /// Distinct slots each keep their own position.
    #[test]
    fn fingers_do_not_merge_each_other() {
        let mut c = InputCoalesce::new();
        c.push(touch(0, 1, 1));
        c.push(touch(1, 2, 2));
        c.push(touch(0, 3, 3));

        assert_eq!(c.drain_front(10), vec![touch(0, 3, 3), touch(1, 2, 2)]);
    }

    /// The key set is bounded by recognised keys, not by what the client sends:
    /// a flood of invented keycodes collapses to one entry instead of allocating
    /// a string-sized entry per event.
    #[test]
    fn unmapped_keycodes_collapse_to_a_single_key() {
        let mut c = InputCoalesce::new();
        for i in 0..10_000 {
            c.push(key(&format!("Bogus{i}")));
        }
        assert_eq!(c.len(), 1);
    }

    /// FIFO front-draining: the front leaves first, the tail survives.
    #[test]
    fn drain_front_is_fifo_and_leaves_the_tail() {
        let mut c = InputCoalesce::new();
        c.push(ClientDatagram::MouseMove { dx: 1, dy: 1 });
        c.push(key("KeyA"));
        c.push(ClientDatagram::Scroll { dx: 2, dy: 2 });

        assert_eq!(
            c.drain_front(1),
            vec![ClientDatagram::MouseMove { dx: 1, dy: 1 }]
        );
        assert_eq!(c.len(), 2);
        assert_eq!(
            c.drain_front(10),
            vec![key("KeyA"), ClientDatagram::Scroll { dx: 2, dy: 2 }]
        );
    }

    /// An empty coalesce consumes nothing.
    #[test]
    fn an_empty_coalesce_drains_nothing() {
        assert!(InputCoalesce::new().drain_budgeted().is_empty());
    }

    #[test]
    fn budget_refills_lazily_and_caps_at_the_burst() {
        let now = Instant::now();
        let mut spent = Budget {
            tokens: 0.0,
            last: now,
        };
        // Half a second at 250/s is far past the cap, so the grant is the cap.
        assert_eq!(
            spent.grant(1000, now + Duration::from_millis(500)),
            INPUT_BURST as usize
        );

        // From dry, a little over one tick grants one event: the sustained rate.
        let mut slow = Budget {
            tokens: 0.0,
            last: now,
        };
        assert_eq!(slow.grant(10, now + Duration::from_millis(10)), 2);
    }

    /// A modifier code and an unmapped one share the zero identity, so neither
    /// can grow the key set.
    #[test]
    fn modifier_and_unknown_codes_share_the_zero_identity() {
        assert_eq!(InputKey::of(&key("ShiftLeft")), InputKey::of(&key("Bogus")));
        assert_eq!(InputKey::of(&key("Bogus")), Some(InputKey::Keyboard(0)));
    }
}
