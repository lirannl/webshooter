//! Ordering and loss policy for the client's video receive path.
//!
//! The server sends a keyframe on its own reliable stream and the delta frames
//! after it as datagrams, because a keyframe is both too large to fragment
//! across datagrams and too important to lose, while a delta frame is
//! disposable. The two transports are not ordered against each other, so the
//! client can be handed a delta that is inter-predicted *from* a keyframe it has
//! not received yet — and, because a keyframe takes far longer to cross the
//! network than the deltas behind it, regularly so.
//!
//! [`FrameGate`] is the policy that makes the split safe, driven entirely by the
//! frame ids (no timers, so no added latency in the steady state):
//!
//! - a frame whose id is behind the highest one seen is stale and dropped, so a
//!   late fragment cannot resurrect a frame that was already moved past —
//!   *unless* the client asked for that frame to be resent, in which case it is a
//!   repair coming back and is exactly what is wanted;
//! - a forward jump in ids is positive evidence that frames were lost, so the
//!   peer is asked to resend them and every frame behind the gap is held back
//!   rather than decoded against a hole;
//! - the frames that close a gap are released in *frame* order, not arrival
//!   order, because a repaired frame arrives after the newer frames it precedes.
//!
//! Keyframes are the one exception to the staleness rule: when the client is
//! waiting for one it arrives *behind* the frames it was predicted from, and
//! discarding it there would leave the client dropping every delta until the
//! next scheduled keyframe seconds later. See [`FrameGate::admit`].

use crate::codec::Codec;
use std::collections::{HashMap, HashSet};

/// How many frames may be held back while a gap is outstanding.
///
/// A keyframe of a 1080p screen at this project's bitrate is a couple of
/// megabytes, so on a slow link the client can outrun the transfer by a second
/// or more of frames before it gives up. The bound exists so a gap that never
/// closes cannot grow the queue without limit; the frames it drops are the
/// *newest* ones, because the frames nearest the gap are the ones that can still
/// be decoded once it closes.
const HELD_LIMIT: usize = 256;

/// A frame held back because it is inter-predicted from a frame that has not
/// arrived yet.
pub struct HeldFrame {
    pub frame_id: u16,
    pub codec: Codec,
    pub payload: Vec<u8>,
}

/// What the receive path must do with a frame it has just assembled.
pub enum FrameAction {
    /// The frame is behind the highest id seen and was not asked for: drop it.
    /// Re-decoding it would feed the decoder a frame it has already moved past.
    Drop,
    /// Decode the frame straight away.
    Decode,
    /// The frame is inter-predicted from a frame that has not arrived, so keep it
    /// (hand it to [`FrameGate::hold`]) and do not decode it. `request` is set on
    /// the first frame of a gap, so the peer is asked to resend the missing
    /// frames once rather than once per frame.
    Hold { request: bool },
    /// The sequence is whole again — a keyframe landed, or a repair closed the
    /// gap. Decode the frame, then decode `held` in the order given: the frames
    /// that were waiting for exactly this point in the sequence.
    Resync { held: Vec<HeldFrame> },
}

/// Client-side video frame ordering and loss bookkeeping.
#[derive(Default)]
pub struct FrameGate {
    /// Frame ids assembled since the last keyframe, used as the positive
    /// evidence that distinguishes a lost frame from a merely later one.
    seen: HashSet<u16>,
    /// The highest frame id assembled, in the wrapping u16 sequence's own order.
    ///
    /// This is how far the *sequence* has progressed, which is not the same as
    /// how far the decoder has: a gap leaves it ahead of [`FrameGate::frontier`]
    /// until the gap closes.
    high: Option<u16>,
    /// The last frame id actually decoded, which only ever moves forward
    /// contiguously.
    ///
    /// This is the decode frontier, and it is what a held frame is released
    /// against. Keeping it separate from `high` is what lets a repaired frame be
    /// accepted when it arrives behind `high`: it is ahead of the frontier, so it
    /// is not stale, and it is expected, so it is not noise.
    frontier: Option<u16>,
    /// Whether some frame is still missing, so deltas must not be decoded until
    /// the gap closes.
    waiting: bool,
    /// Assembled frames held back until the gap they depend on closes, keyed by
    /// frame id.
    ///
    /// A map rather than a queue because the release order is the frame order
    /// and a repaired frame arrives *after* the newer frames it precedes: an
    /// arrival-ordered queue would replay them backwards.
    held: HashMap<u16, HeldFrame>,
    /// Frame ids the client has asked the peer to resend and is still expecting.
    ///
    /// This is what separates a repair coming back from a late fragment of a
    /// frame the client has already given up on. Both arrive behind `high`; only
    /// the first is wanted.
    awaiting: HashSet<u16>,
    /// Frame ids the most recent [`FrameGate::admit`] call found missing.
    ///
    /// Kept rather than returned because a gap is the one thing in this type
    /// whose cause cannot be inferred from here: the ids were either never sent,
    /// dropped in transit, or dropped by the receiver after arriving. Reporting
    /// them is the only way to tell those apart, and every one of them costs a
    /// resend or a keyframe — which on a slow link is the expensive part.
    missing: Vec<u16>,
}

/// Whether `a` is at or behind `b` in the wrapping u16 frame sequence. Half the
/// sequence is "behind", which is what makes a forward jump distinguishable from
/// an out-of-order or wrapped frame.
fn is_older_or_equal(a: u16, b: u16) -> bool {
    b.wrapping_sub(a) < 0x8000
}

impl FrameGate {
    /// Classify a frame that has just been assembled from all its fragments.
    ///
    /// A caller that gets [`FrameAction::Hold`] must pass the frame's bytes to
    /// [`FrameGate::hold`]; a caller that gets [`FrameAction::Resync`] must
    /// decode the frame before the `held` frames it is handed back.
    pub fn admit(&mut self, frame_id: u16, is_keyframe: bool) -> FrameAction {
        if is_keyframe {
            // A keyframe is the only frame that can rebuild a broken chain, so
            // it is never dropped as stale while one is outstanding — the
            // frames it overtook are held back below, not lost. Only a keyframe
            // that is behind the decode frontier *and* unneeded is stale.
            if !self.waiting
                && self
                    .frontier
                    .is_some_and(|frontier| is_older_or_equal(frame_id, frontier))
            {
                return FrameAction::Drop;
            }
            self.seen.clear();
            self.seen.insert(frame_id);
            self.awaiting.clear();
            self.high = Some(frame_id);
            self.frontier = Some(frame_id);
            self.waiting = false;
            let held = self.drain_from(frame_id.wrapping_add(1));
            // Whatever is left is not predicted from this keyframe, so it can
            // never be decoded: the frames behind the gap were predicted from
            // frames that never arrived. Dropping them here is what stops the
            // queue from carrying frames the next keyframe will not rescue.
            if !self.held.is_empty() {
                log::debug!(
                    "dropping {} frames held behind keyframe {}: not predicted from it",
                    self.held.len(),
                    frame_id
                );
                self.held.clear();
            }
            return FrameAction::Resync { held };
        }

        // Behind the highest id seen. Two things arrive here: a repair the client
        // asked for, and a late fragment of a frame it has already given up on.
        // They are told apart by whether the request is still outstanding —
        // re-decoding the second would feed the decoder a frame it has moved
        // past, which is the one outcome this type exists to prevent.
        if self
            .high
            .is_some_and(|high| is_older_or_equal(frame_id, high))
        {
            return if self.awaiting.remove(&frame_id) {
                FrameAction::Hold { request: false }
            } else {
                FrameAction::Drop
            };
        }

        // Ahead of the highest id seen: either the next frame in order, or
        // evidence of a gap. The gap is only a loss if the ids inside it were
        // never assembled, so a frame that merely overtook the one in progress
        // is not a gap.
        self.missing.clear();
        if let Some(high) = self.high {
            let diff = frame_id.wrapping_sub(high);
            if diff != 0 && diff < 0x8000 {
                self.missing = (1..diff)
                    .map(|missing| high.wrapping_add(missing))
                    .filter(|missing| !self.seen.contains(missing))
                    .collect();
            }
        }
        // The first frame ever is a delta: there is nothing to predict it from,
        // so the client cannot decode anything until a keyframe lands.
        if self.high.is_none() {
            self.high = Some(frame_id);
            self.seen.insert(frame_id);
            self.waiting = true;
            return FrameAction::Hold { request: true };
        }

        self.high = Some(frame_id);
        self.seen.insert(frame_id);

        if !self.missing.is_empty() {
            // A gap. The client asks for the missing frames to be resent and
            // escalates to a keyframe if they do not come back, so from here
            // every frame is held rather than decoded against a hole.
            self.waiting = true;
            return FrameAction::Hold { request: true };
        }

        if self.waiting {
            // Still a hole somewhere behind, so this frame cannot be decoded yet
            // even though it is the next id in order.
            return FrameAction::Hold { request: false };
        }

        // The next frame in order and nothing missing: decode it, then release
        // whatever has become decodable behind it. That run is usually empty, and
        // is not when a repair has just closed a gap.
        self.frontier = Some(frame_id);
        let held = self.drain_from(frame_id.wrapping_add(1));
        if held.is_empty() {
            FrameAction::Decode
        } else {
            FrameAction::Resync { held }
        }
    }

    /// Declare that the client has asked for `frame_id` to be resent.
    ///
    /// Without this, a frame that arrives behind [`FrameGate::high`] is stale and
    /// dropped. With it, the frame is accepted when it comes back — which is the
    /// whole mechanism by which a repaired frame re-enters the sequence.
    ///
    /// Called once per gap, with every id the gap named, so that a repair which
    /// only partly closes the gap still has its frames recognised.
    pub fn expect_repair(&mut self, frame_ids: &[u16]) {
        self.awaiting.extend(frame_ids.iter().copied());
    }

    /// Keep a frame the gate answered with [`FrameAction::Hold`], to be decoded
    /// once the gap it depends on closes.
    ///
    /// Returns the run of frames that holding this one made decodable, which is
    /// empty unless the frame was the one the gap was waiting for — a repaired
    /// frame arriving after the newer ones it precedes closes the gap and
    /// releases all of them at once.
    pub fn hold(&mut self, frame_id: u16, codec: Codec, payload: Vec<u8>) -> Vec<HeldFrame> {
        if self.held.len() >= HELD_LIMIT {
            // The newest frame is dropped rather than the oldest: the frames
            // nearest the gap are the ones that can still be decoded when it
            // closes. "Newest" is the largest forward distance from the decode
            // frontier, which is what keeps the run behind it contiguous.
            let Some(&newest) = self
                .held
                .keys()
                .max_by_key(|id| id.wrapping_sub(self.frontier.unwrap_or(**id)))
            else {
                return Vec::new();
            };
            self.held.remove(&newest);
        }
        self.held.insert(
            frame_id,
            HeldFrame {
                frame_id,
                codec,
                payload,
            },
        );
        // Holding a frame can close the gap: a repair arrives behind the frames
        // it precedes, so the run it releases starts at the frontier, not here.
        let from = self.frontier.map(|frontier| frontier.wrapping_add(1));
        match from {
            Some(from) => self.drain_from(from),
            None => Vec::new(),
        }
    }

    /// How many frames are currently held back.
    pub fn held_count(&self) -> usize {
        self.held.len()
    }

    /// The frame ids the most recent [`FrameGate::admit`] found missing.
    ///
    /// Empty unless that call detected a gap. Read it right after a
    /// [`FrameAction::Hold`] that carries `request`, which is the only time a
    /// gap starts.
    pub fn missing(&self) -> &[u16] {
        &self.missing
    }

    /// Whether the client is still expecting a resent frame to arrive.
    ///
    /// The caller uses this to decide whether a gap can be repaired at all, and
    /// to tell a repair that came back from one that has been abandoned.
    pub fn awaiting_repair(&self) -> bool {
        !self.awaiting.is_empty()
    }

    /// The run of held frames starting at `start`, in frame order.
    ///
    /// Walks forward from `start` while the next id is held, removing each as it
    /// goes, and stops at the first id that is not. Whatever is left in the map
    /// is unreachable from `start` and stays held — or is dropped by the caller
    /// once it is clear the gap will never close.
    fn drain_from(&mut self, start: u16) -> Vec<HeldFrame> {
        let mut released = Vec::new();
        let mut expected = start;
        while let Some(frame) = self.held.remove(&expected) {
            self.seen.insert(frame.frame_id);
            self.frontier = Some(frame.frame_id);
            expected = expected.wrapping_add(1);
            released.push(frame);
        }
        // The gap is closed when the decode frontier has caught up with the
        // highest id seen. That is the moment a repair has worked, and it is
        // what lets the next frame decode normally rather than being held. The
        // expectations are spent by definition: every id the client asked for
        // has either arrived or been passed over by the frontier.
        if self.frontier == self.high {
            self.waiting = false;
            self.awaiting.clear();
        }
        released
    }

    /// Whether the client is still expecting `frame_id` to be resent.
    ///
    /// The receive path uses this to decide which incomplete frames to keep: a
    /// frame behind the one being presented is finished with, unless its
    /// fragments are still on their way back.
    pub fn is_awaiting(&self, frame_id: u16) -> bool {
        self.awaiting.contains(&frame_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain keyframe-then-deltas run decodes in order, with no request and
    /// nothing held back.
    #[test]
    fn a_clean_run_decodes_every_frame() {
        let mut gate = FrameGate::default();
        assert!(matches!(
            gate.admit(100, true),
            FrameAction::Resync { held } if held.is_empty()
        ));
        for id in 101..110 {
            assert!(
                matches!(gate.admit(id, false), FrameAction::Decode),
                "delta {id} should decode"
            );
        }
        assert_eq!(gate.held_count(), 0);
    }

    /// A forward jump is the loss evidence: the first frame behind the gap asks
    /// for the missing frames, the rest of them are held, and none is decoded.
    #[test]
    fn a_gap_is_held_and_asks_for_a_resend_once() {
        let mut gate = FrameGate::default();
        gate.admit(10, true);
        assert!(matches!(gate.admit(11, false), FrameAction::Decode));
        assert!(matches!(
            gate.admit(15, false),
            FrameAction::Hold { request: true }
        ));
        assert!(gate.hold(15, Codec::Av1, vec![15]).is_empty());
        assert!(matches!(
            gate.admit(16, false),
            FrameAction::Hold { request: false }
        ));
        assert!(gate.hold(16, Codec::Av1, vec![16]).is_empty());
        assert_eq!(gate.held_count(), 2);
    }

    /// The case the split transport exists for: the deltas behind a keyframe
    /// arrive first, so the keyframe lands *behind* the frames it overtook. It
    /// must still be decoded — dropping it would leave the client discarding
    /// every delta until the next scheduled keyframe — and the frames it overtook
    /// must come back out in order behind it, with the loss detector
    /// resynchronised so it does not ask for a resend that just arrived.
    #[test]
    fn an_overtaking_keyframe_resyncs_and_replays_the_held_frames() {
        let mut gate = FrameGate::default();
        gate.admit(19, true);
        // 21, 22, 23 arrive as datagrams while the keyframe 20 is still on the
        // wire, so each of them lands behind a gap at 20.
        for id in 21..=23 {
            assert!(matches!(gate.admit(id, false), FrameAction::Hold { .. }));
            assert!(gate.hold(id, Codec::Av1, vec![id as u8]).is_empty());
        }
        // The keyframe arrives with an id behind the last assembled one.
        let FrameAction::Resync { held } = gate.admit(20, true) else {
            panic!("an awaited keyframe must never be dropped as stale");
        };
        let ids: Vec<u16> = held.iter().map(|f| f.frame_id).collect();
        assert_eq!(ids, vec![21, 22, 23], "held frames replay in frame order");
        assert_eq!(gate.held_count(), 0);
        // The chain is whole again: 24 follows 23 directly, so it decodes and
        // no further resend is requested.
        assert!(matches!(gate.admit(24, false), FrameAction::Decode));
    }

    /// A keyframe that overtook nothing is stale and dropped, so a late copy
    /// cannot roll the decoder back over frames already on screen.
    #[test]
    fn a_keyframe_behind_the_last_frame_is_dropped_while_in_sync() {
        let mut gate = FrameGate::default();
        gate.admit(20, true);
        assert!(matches!(gate.admit(21, false), FrameAction::Decode));
        assert!(matches!(gate.admit(22, false), FrameAction::Decode));
        assert!(
            matches!(gate.admit(18, true), FrameAction::Drop),
            "a keyframe behind the last assembled frame adds nothing"
        );
    }

    /// A keyframe that is *newer* than the frames waiting behind the lost one
    /// releases none of them: they are predicted from a frame that never
    /// arrived, so they can never be decoded. They are dropped and decoding
    /// resumes from the new keyframe, without another resend being asked for.
    #[test]
    fn a_keyframe_after_the_held_frames_releases_none_of_them() {
        let mut gate = FrameGate::default();
        gate.admit(20, true);
        // 30 and 31 land behind a gap at 29 — the keyframe they need.
        for id in 30..=31 {
            assert!(matches!(gate.admit(id, false), FrameAction::Hold { .. }));
            assert!(gate.hold(id, Codec::Av1, vec![id as u8]).is_empty());
        }
        // A *newer* keyframe arrives instead, so 30 and 31 are unreachable.
        let FrameAction::Resync { held } = gate.admit(35, true) else {
            panic!("a fresh keyframe must rebuild the chain");
        };
        assert!(held.is_empty(), "30 and 31 are not predicted from 35");
        assert_eq!(gate.held_count(), 0, "the unreachable frames are dropped");
        // 36 follows the keyframe directly: the chain is whole again, so no
        // further resend is requested.
        assert!(matches!(gate.admit(36, false), FrameAction::Decode));
    }

    /// A frame that is behind the highest id seen is stale, and a duplicate
    /// fragment of it must not resurrect it.
    #[test]
    fn frames_behind_the_highest_id_are_dropped() {
        let mut gate = FrameGate::default();
        gate.admit(10, true);
        assert!(matches!(gate.admit(11, false), FrameAction::Decode));
        assert!(matches!(gate.admit(10, false), FrameAction::Drop));
        assert!(matches!(gate.admit(9, false), FrameAction::Drop));
    }

    /// The first frame of a session is a delta more often than not: there is
    /// nothing to predict it from, so it is held and a resend is requested.
    #[test]
    fn a_first_delta_frame_is_held_and_asks_for_a_resend() {
        let mut gate = FrameGate::default();
        assert!(matches!(
            gate.admit(0, false),
            FrameAction::Hold { request: true }
        ));
        assert!(gate.hold(0, Codec::Av1, vec![0]).is_empty());
        assert!(matches!(
            gate.admit(1, false),
            FrameAction::Hold { request: false }
        ));
    }

    /// The frame sequence is a wrapping u16 counter; the ordering must survive
    /// the wrap, and a keyframe that arrives across it must release the frames
    /// that follow it.
    #[test]
    fn frame_id_wraparound_keeps_the_order() {
        let mut gate = FrameGate::default();
        assert!(matches!(
            gate.admit(0xFFFE, true),
            FrameAction::Resync { .. }
        ));
        assert!(matches!(gate.admit(0xFFFF, false), FrameAction::Decode));
        // 0 is the next frame in the sequence, not one behind 0xFFFF.
        assert!(matches!(gate.admit(0, false), FrameAction::Decode));
        assert!(
            matches!(gate.admit(0xFF00, true), FrameAction::Drop),
            "0xFF00 is far behind 0 in the wrapping sequence"
        );
        // A keyframe that overtook 1 and 2 while they were still in flight.
        assert!(matches!(
            gate.admit(4, false),
            FrameAction::Hold { request: true }
        ));
        assert!(gate.hold(4, Codec::Av1, vec![4]).is_empty());
        assert!(matches!(gate.admit(5, false), FrameAction::Hold { .. }));
        assert!(gate.hold(5, Codec::Av1, vec![5]).is_empty());
        let FrameAction::Resync { held } = gate.admit(3, true) else {
            panic!("an awaited keyframe must never be dropped as stale");
        };
        let ids: Vec<u16> = held.iter().map(|f| f.frame_id).collect();
        assert_eq!(
            ids,
            vec![4, 5],
            "the replay follows the keyframe across the wrap"
        );
    }

    /// A keyframe that never arrives must not grow the queue without limit, and
    /// the frames that survive must be the ones a later keyframe can decode: the
    /// queue overflows at the newest end, so what is left is the run that
    /// follows the gap directly.
    #[test]
    fn the_hold_queue_is_bounded_and_keeps_the_frames_nearest_a_gap() {
        let mut gate = FrameGate::default();
        gate.admit(0, true);
        assert!(matches!(gate.admit(1, false), FrameAction::Decode));
        // The keyframe at 999 is still in flight when the frames behind it
        // arrive, so each of them lands behind a gap at 999 and is held.
        let first = 1000u16;
        let queued = HELD_LIMIT as u16 + 8;
        for id in first..=first + queued {
            assert!(matches!(gate.admit(id, false), FrameAction::Hold { .. }));
            assert!(gate.hold(id, Codec::Av1, vec![0]).is_empty());
        }
        assert_eq!(gate.held_count(), HELD_LIMIT, "the queue is bounded");
        let FrameAction::Resync { held } = gate.admit(999, true) else {
            panic!("an awaited keyframe must never be dropped as stale");
        };
        assert_eq!(
            held.first().map(|f| f.frame_id),
            Some(first),
            "the frames nearest the gap are the ones kept"
        );
        assert_eq!(
            held.len(),
            HELD_LIMIT - 1,
            "the run is bounded by the queue, not by the frames that arrived"
        );
        assert_eq!(
            held.last().map(|f| f.frame_id),
            Some(first + held.len() as u16 - 1),
            "only a contiguous run behind the keyframe is released"
        );
        assert_eq!(gate.held_count(), 0, "the undecodable tail is dropped");
    }

    /// A repaired frame arrives *behind* the highest id seen and *after* the
    /// newer frames it precedes. It must be accepted rather than dropped as
    /// stale, and holding it must release the whole run in frame order — which is
    /// the case an arrival-ordered queue would get wrong.
    #[test]
    fn a_repaired_frame_is_accepted_and_releases_the_run_in_frame_order() {
        let mut gate = FrameGate::default();
        gate.admit(100, true);
        for id in 101..=103 {
            assert!(matches!(gate.admit(id, false), FrameAction::Decode));
        }
        // 104 and 105 lose a fragment each; 106 arrives complete and reveals the
        // gap. The client asks for 104 and 105 to be resent.
        assert!(matches!(
            gate.admit(106, false),
            FrameAction::Hold { request: true }
        ));
        gate.expect_repair(&[104, 105]);
        assert!(gate.hold(106, Codec::Av1, vec![106]).is_empty());

        // The repairs come back in the wrong order: 105 first, then 104. Both
        // are behind the highest id seen, so without the expectation they would
        // be dropped as stale.
        assert!(matches!(
            gate.admit(105, false),
            FrameAction::Hold { request: false }
        ));
        assert!(
            gate.hold(105, Codec::Av1, vec![105]).is_empty(),
            "105 alone does not close the gap: 104 is still missing"
        );
        assert_eq!(gate.held_count(), 2, "105 and 106 are both held");

        assert!(matches!(
            gate.admit(104, false),
            FrameAction::Hold { request: false }
        ));
        let released = gate.hold(104, Codec::Av1, vec![104]);
        let ids: Vec<u16> = released.iter().map(|f| f.frame_id).collect();
        assert_eq!(
            ids,
            vec![104, 105, 106],
            "the run is released in frame order, not arrival order"
        );
        assert_eq!(gate.held_count(), 0);
        assert!(!gate.awaiting_repair(), "the expectation is spent");
        // The chain is whole again: 107 follows 106 directly.
        assert!(matches!(gate.admit(107, false), FrameAction::Decode));
    }

    /// A repair that is abandoned must not leave the gate expecting a frame
    /// forever: a late fragment of a frame the client has given up on is stale,
    /// and accepting it would feed the decoder a frame it has moved past.
    #[test]
    fn a_frame_arriving_after_its_repair_was_abandoned_is_stale() {
        let mut gate = FrameGate::default();
        gate.admit(100, true);
        assert!(matches!(
            gate.admit(102, false),
            FrameAction::Hold { request: true }
        ));
        gate.expect_repair(&[101]);
        assert!(gate.hold(102, Codec::Av1, vec![102]).is_empty());

        // The client gives up on 101 and asks for a keyframe instead. The
        // expectation is spent, so a late fragment of 101 is stale.
        gate.admit(200, true);
        assert!(
            matches!(gate.admit(101, false), FrameAction::Drop),
            "a frame whose repair was abandoned must not be resurrected"
        );
    }

    /// A gap that is only partly repairable still holds the rest: the frames the
    /// client could not name a fragment for are not decodable, so nothing behind
    /// the gap may be decoded until a keyframe arrives.
    #[test]
    fn a_partially_repaired_gap_still_holds_what_it_cannot_repair() {
        let mut gate = FrameGate::default();
        gate.admit(100, true);
        // 101 is lost outright (never sent), 102 loses a fragment.
        assert!(matches!(
            gate.admit(103, false),
            FrameAction::Hold { request: true }
        ));
        gate.expect_repair(&[102]);
        assert!(gate.hold(103, Codec::Av1, vec![103]).is_empty());

        // 102 comes back, but 101 is still missing, so the run cannot start.
        assert!(matches!(
            gate.admit(102, false),
            FrameAction::Hold { request: false }
        ));
        assert!(
            gate.hold(102, Codec::Av1, vec![102]).is_empty(),
            "102 is held because 101 is still missing"
        );
        assert_eq!(gate.held_count(), 2);
        // A keyframe is the only thing that can close a gap repair cannot.
        let FrameAction::Resync { held } = gate.admit(150, true) else {
            panic!("a keyframe must rebuild the chain");
        };
        assert!(held.is_empty(), "102 and 103 are not predicted from 150");
        assert!(matches!(gate.admit(151, false), FrameAction::Decode));
    }
}
