//! Lock-free single-producer / single-consumer ring buffer for encoded frames.
//!
//! The encoder (a PipeWire/Vulkan thread) and the WebTransport forwarder (a
//! Tokio task) are the only two participants, so an SPSC ring is both the
//! lowest-latency handoff available and the easiest to reason about: two
//! monotonically increasing sequence numbers, a fixed pre-allocated slot
//! array, and memory-ordering fences instead of locks.
//!
//! Zero-copy semantics: [`EncodedFrame`] owns its compressed bitstream in a
//! `Vec<u8>` and only that owning handle is moved through the ring — the frame
//! bytes themselves never cross. Vk-encoder output is copied into a fresh
//! `data` buffer exactly once (GPU → host), then the handle travels the ring
//! by pointer move.
//!
//! # Ordering rules
//!
//! * Producer (`push`): writes slot, then `tail.store(.., Release)` so the
//!   consumer's `tail.load(Acquire)` publishes the slot contents.
//! * Consumer (`pop`): reads slot, then `head.store(.., Release)` so the
//!   producer's `head.load(Acquire)` publishes that the slot is free again.
//! * Both threads only ever touch disjoint `head`/`tail` sequences; no slot is
//!   written by the producer while the consumer may still be reading it and
//!   vice versa (guaranteed by the full/empty checks below).

use shared::codec::Codec;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};

/// A fully encoded frame ready for the transport layer.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Compressed codec bitstream (an Annex-B AV1 / H.264 / H.265 stream).
    pub data: Vec<u8>,
    /// True if the decoder can start/resync from this frame without any prior
    /// state (IDR for H.264/H.265, key frame for AV1).
    pub is_keyframe: bool,
    /// Codec the payload was encoded with.
    pub codec: Codec,
}

/// Lock-free SPSC ring buffer of [`EncodedFrame`], shareable between threads
/// via `Arc`. The producer is the encoder thread; the consumer is the
/// forwarder task.
pub struct FrameRing {
    slots: Box<[UnsafeCell<Option<EncodedFrame>>]>,
    mask: u64,
    /// Producer index: number of pushes ever made (monotonic, wraps into
    /// `slots` via `mask`).
    tail: AtomicU64,
    /// Consumer index: number of pops ever made.
    head: AtomicU64,
}

// SAFETY: UnsafeCell is only accessed via the atomic head/tail discipline
// above; EncodedFrame is Send, and each slot is owned by exactly one of the
// two threads at any instant.
unsafe impl Send for FrameRing {}
unsafe impl Sync for FrameRing {}

impl FrameRing {
    /// Create a ring of `capacity` slots. Capacity is rounded up to a power of
    /// two so `idx = seq & mask` is the slot index.
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1).next_power_of_two();
        let slots = (0..capacity)
            .map(|_| UnsafeCell::new(None))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            slots,
            mask: (capacity as u64) - 1,
            tail: AtomicU64::new(0),
            head: AtomicU64::new(0),
        }
    }

    /// Number of frames currently queued (advisory; for diagnostics only).
    pub fn len(&self) -> usize {
        (self.tail.load(Ordering::Acquire) - self.head.load(Ordering::Acquire)) as usize
    }

    /// `true` if the ring has no free slot for another frame.
    pub fn is_full(&self) -> bool {
        self.tail.load(Ordering::Acquire) - self.head.load(Ordering::Acquire)
            >= self.slots.len() as u64
    }

    /// `true` if the ring currently holds no frames.
    pub fn is_empty(&self) -> bool {
        self.tail.load(Ordering::Acquire) == self.head.load(Ordering::Acquire)
    }

    /// Enqueue a frame. Returns `Err(frame)` when full so the caller can count
    /// the drop (the encoder sheds frames rather than blocking the capture).
    ///
    /// **Producer-only.**
    pub fn push(&self, frame: EncodedFrame) -> Result<(), EncodedFrame> {
        let tail = self.tail.load(Ordering::Relaxed);
        // Acquire: pairs with the consumer's Release head store so a slot the
        // consumer just freed is visible to us.
        let head = self.head.load(Ordering::Acquire);
        if tail - head >= self.slots.len() as u64 {
            return Err(frame);
        }
        let slot = unsafe { &mut *self.slots[(tail & self.mask) as usize].get() };
        debug_assert!(slot.is_none(), "SPSC violated: producer overwrote a live slot");
        *slot = Some(frame);
        // Publish the slot contents before the tail bump is visible.
        std::sync::atomic::fence(Ordering::Release);
        self.tail.store(tail + 1, Ordering::Release);
        Ok(())
    }

    /// Take the oldest frame, or `None` if empty.
    ///
    /// **Consumer-only.**
    pub fn pop(&self) -> Option<EncodedFrame> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let slot = unsafe { &mut *self.slots[(head & self.mask) as usize].get() };
        // Acquire from the producer's Release tail store covers the slot write.
        let frame = slot.take().expect("ring slot was empty despite tail > head");
        // Free the slot for the producer only after we've taken the value.
        std::sync::atomic::fence(Ordering::Release);
        self.head.store(head + 1, Ordering::Release);
        Some(frame)
    }

    /// Drain the ring, returning every queued frame in FIFO order.
    /// Producer/consumer-independent diagnostic helper.
    pub fn drain(&self) -> Vec<EncodedFrame> {
        let mut out = Vec::new();
        while let Some(frame) = self.pop() {
            out.push(frame);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn frame(n: u8) -> EncodedFrame {
        EncodedFrame {
            data: vec![n; 4],
            is_keyframe: n == 0,
            codec: Codec::Av1,
        }
    }

    #[test]
    fn fifo_order() {
        let ring = FrameRing::with_capacity(4);
        ring.push(frame(1)).unwrap();
        ring.push(frame(2)).unwrap();
        ring.push(frame(3)).unwrap();
        assert!(ring.pop().unwrap().data == vec![1; 4]);
        assert!(ring.pop().unwrap().data == vec![2; 4]);
        assert!(ring.pop().unwrap().data == vec![3; 4]);
        assert!(ring.pop().is_none());
    }

    #[test]
    fn full_returns_err_and_keeps_oldest() {
        let ring = FrameRing::with_capacity(2);
        ring.push(frame(1)).unwrap();
        ring.push(frame(2)).unwrap();
        let rejected = ring.push(frame(3)).unwrap_err();
        assert_eq!(rejected.data, vec![3; 4]);
        assert_eq!(ring.pop().unwrap().data, vec![1; 4]);
        assert_eq!(ring.pop().unwrap().data, vec![2; 4]);
    }

    #[test]
    fn wraps_around_capacity() {
        let ring = FrameRing::with_capacity(2);
        for n in 0..6u8 {
            ring.push(frame(n)).unwrap();
            let popped = ring.pop().unwrap();
            assert_eq!(popped.data, vec![n; 4]);
        }
        assert!(ring.is_empty());
    }

    #[test]
    fn concurrent_producer_consumer() {
        const N: u64 = 10_000;
        let ring = Arc::new(FrameRing::with_capacity(256));
        let prod = ring.clone();
        let consumer = thread::spawn(move || {
            let mut seen = 0u64;
            let mut last = None;
            while seen < N {
                if let Some(f) = prod.pop() {
                    let got = u64::from_le_bytes(f.data[..8].try_into().unwrap());
                    if let Some(prev) = last {
                        assert!(got > prev, "out-of-order pop: {prev} then {got}");
                    }
                    last = Some(got);
                    seen += 1;
                } else {
                    thread::yield_now();
                }
            }
            seen
        });
        let mut next = 0u64;
        while next < N {
            if ring.push(EncodedFrame {
                data: next.to_le_bytes().to_vec(),
                is_keyframe: false,
                codec: Codec::Av1,
            }).is_err() {
                thread::yield_now();
            } else {
                next += 1;
            }
        }
        assert_eq!(consumer.join().unwrap(), N);
    }
}