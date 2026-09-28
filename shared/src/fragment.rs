/// Incremental reassembly of a single multi-fragment frame.
///
/// Callers own the `frame_id -> FragmentFrame` mapping — including *when* an
/// entry is evicted, which deliberately differs per consumer (the video render
/// loop drops entries wholesale for frames it has moved past, the audio player
/// only removes completed frames). This type only tracks the fragment slots of
/// one frame, so the accounting logic shared by both receive paths cannot
/// drift apart.
pub struct FragmentFrame {
    fragments: Vec<Option<Vec<u8>>>,
    received: usize,
}

/// What [`FragmentFrame::push`] did with a fragment.
pub enum PushOutcome {
    /// Slot `index` is out of range for this frame's fragment count.
    OutOfRange,
    /// Slot `index` was already filled (duplicate fragment).
    Duplicate,
    /// Fragment accepted, but the frame is still missing fragments.
    Incomplete,
    /// Frame complete: the assembled payload bytes.
    Complete(Vec<u8>),
}

impl FragmentFrame {
    pub fn new(num_frags: usize) -> Self {
        Self {
            fragments: vec![None; num_frags],
            received: 0,
        }
    }

    /// Number of fragments the frame is declared to have. Used by callers to
    /// detect a stale frame_id colliding with a new frame (the u16 frame
    /// counter wraps): a changed count means the entry belongs to a previous
    /// use of the same id.
    pub fn num_frags(&self) -> usize {
        self.fragments.len()
    }

    /// The fragment indices that have not arrived, in ascending order.
    ///
    /// This is what turns "a frame is incomplete" into something actionable.
    /// A caller that only knows a frame is short of fragments has one option —
    /// give up on it and ask for a keyframe — but the empty slots name
    /// themselves, and the server can resend exactly those. Losing three
    /// fragments of a frame and losing the whole frame are the same event to
    /// anyone watching, and they should not cost the same to recover from.
    ///
    /// Empty once the frame is complete, and for a frame declared to have no
    /// fragments at all.
    pub fn missing(&self) -> Vec<u16> {
        self.fragments
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_none())
            .map(|(index, _)| index as u16)
            .collect()
    }

    /// Record fragment `index`. Out-of-range and duplicate fragments leave the
    /// frame untouched and are reported as such so each caller applies its own
    /// entry-lifetime policy (drop the frame vs. keep waiting).
    pub fn push(&mut self, index: usize, payload: Vec<u8>) -> PushOutcome {
        let Some(slot) = self.fragments.get_mut(index) else {
            return PushOutcome::OutOfRange;
        };
        if slot.is_some() {
            return PushOutcome::Duplicate;
        }
        *slot = Some(payload);
        self.received += 1;
        if self.received < self.fragments.len() {
            return PushOutcome::Incomplete;
        }

        let total: usize = self
            .fragments
            .iter()
            .map(|fragment| fragment.as_ref().map_or(0, Vec::len))
            .sum();
        let mut assembled = Vec::with_capacity(total);
        for data in self.fragments.iter().flatten() {
            assembled.extend_from_slice(data);
        }
        PushOutcome::Complete(assembled)
    }
}