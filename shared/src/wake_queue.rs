//! A queue that hands items to poll-based consumers and wakes them on arrival.
//!
//! `moq-net` is sans-I/O: its driver asks for "the next unidirectional stream"
//! through `web_transport_trait::poll::Session::poll_accept_uni` and expects
//! either a stream or `Pending`. The thing producing those streams is an async
//! task (the mux pump, on either side of the session), so something has to
//! bridge a blocking producer to a poll-based consumer. This is that bridge.
//!
//! `tokio::sync::mpsc` would do, but its `Receiver::poll_recv` may only be used
//! while the receiver is exclusively borrowed, and MoQ clones its session so
//! several operations can be pending at once. A queue plus a list of wakers has
//! no such restriction: any number of consumers may wait, and each gets the wake
//! it registered.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

struct Inner<T> {
    items: VecDeque<T>,
    /// Every consumer currently waiting for an item. Deduplicated by `will_wake`
    /// so a consumer that polls in a loop does not accumulate stale clones.
    wakers: Vec<Waker>,
    closed: bool,
}

/// A multi-consumer, multi-producer queue with poll-based pops.
///
/// Producers are [`push`](Self::push) and [`close`](Self::close); consumers are
/// [`poll_pop`](Self::poll_pop). Closing is one-way and is how a consumer learns
/// that no more items are coming, which is what distinguishes a dead session from
/// a slow one.
///
/// Cloneable, because the queue has two kinds of holder that cannot be the same
/// value: the mux pump that feeds it, and the task that drains it. Sharing one
/// `Arc` is what makes both see the same items and the same close.
pub struct WakeQueue<T> {
    inner: Arc<Mutex<Inner<T>>>,
}

impl<T> Clone for WakeQueue<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Default for WakeQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> WakeQueue<T> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                items: VecDeque::new(),
                wakers: Vec::new(),
                closed: false,
            })),
        }
    }

    /// Hand an item to every waiting consumer, and wake them.
    pub fn push(&self, item: T) {
        // The lock is dropped before waking: a woken consumer may immediately
        // poll back into this queue, and holding the lock across `wake` would
        // make that a deadlock rather than a re-entrant poll.
        let wakers = {
            let mut inner = self.lock();
            if inner.closed {
                // Nobody will ever read this. Dropping it here rather than
                // queueing it keeps a dead consumer from growing the queue
                // without bound.
                return;
            }
            inner.items.push_back(item);
            std::mem::take(&mut inner.wakers)
        };
        wake_all(wakers);
    }

    /// Declare that no further items will arrive, and wake every waiter.
    pub fn close(&self) {
        let wakers = {
            let mut inner = self.lock();
            inner.closed = true;
            std::mem::take(&mut inner.wakers)
        };
        wake_all(wakers);
    }

    /// Take an item if there is one, else register `cx`'s waker for the next push.
    ///
    /// `Ready(None)` means the queue is closed *and* drained, which is the only
    /// way a consumer distinguishes "nothing right now" from "never again".
    pub fn poll_pop(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut inner = self.lock();
        if let Some(item) = inner.items.pop_front() {
            return Poll::Ready(Some(item));
        }
        if inner.closed {
            return Poll::Ready(None);
        }
        if !inner.wakers.iter().any(|w| w.will_wake(cx.waker())) {
            inner.wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }

    /// How many items are queued, for tests and diagnostics.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    /// Never let a poisoned lock look like a closed queue: every path here is
    /// short and non-panicking, so poisoning can only come from a caller that
    /// panicked inside [`push`](Self::push)'s critical section, and recovering is
    /// strictly better than tearing the session down.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<T>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn wake_all(wakers: Vec<Waker>) {
    for waker in wakers {
        waker.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{RawWaker, RawWakerVTable};

    /// A waker that flips a flag, so a test can tell whether it was woken without
    /// running the woken task.
    fn flag_waker(flag: Arc<AtomicBool>) -> Waker {
        fn clone(ptr: *const ()) -> RawWaker {
            let arc = unsafe { Arc::from_raw(ptr as *const AtomicBool) };
            let cloned = arc.clone();
            std::mem::forget(arc);
            RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
        }
        fn wake(ptr: *const ()) {
            let arc = unsafe { Arc::from_raw(ptr as *const AtomicBool) };
            arc.store(true, Ordering::SeqCst);
        }
        fn wake_by_ref(ptr: *const ()) {
            let arc = unsafe { Arc::from_raw(ptr as *const AtomicBool) };
            arc.store(true, Ordering::SeqCst);
            std::mem::forget(arc);
        }
        fn drop_waker(ptr: *const ()) {
            drop(unsafe { Arc::from_raw(ptr as *const AtomicBool) });
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);
        let raw = RawWaker::new(Arc::into_raw(flag) as *const (), &VTABLE);
        unsafe { Waker::from_raw(raw) }
    }

    fn poll_once<T>(queue: &WakeQueue<T>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        queue.poll_pop(cx)
    }

    #[test]
    fn a_push_after_a_pending_pop_wakes_the_consumer() {
        let queue: WakeQueue<u8> = WakeQueue::new();
        let flag = Arc::new(AtomicBool::new(false));
        let waker = flag_waker(flag.clone());
        let mut cx = Context::from_waker(&waker);

        assert!(poll_once(&queue, &mut cx).is_pending());
        assert!(!flag.load(Ordering::SeqCst), "nothing to wake for yet");

        queue.push(7);
        assert!(
            flag.load(Ordering::SeqCst),
            "the push must wake the consumer"
        );
        assert_eq!(poll_once(&queue, &mut cx), Poll::Ready(Some(7)));
    }

    /// The distinction the whole type exists for: `Pending` is "not yet" and
    /// `Ready(None)` is "never again". Collapsing them would hang a driver
    /// instead of ending it.
    #[test]
    fn close_drains_before_it_reports_the_end() {
        let queue: WakeQueue<u8> = WakeQueue::new();
        let flag = Arc::new(AtomicBool::new(false));
        let waker = flag_waker(flag.clone());
        let mut cx = Context::from_waker(&waker);

        queue.push(1);
        queue.push(2);
        queue.close();

        assert_eq!(poll_once(&queue, &mut cx), Poll::Ready(Some(1)));
        assert_eq!(poll_once(&queue, &mut cx), Poll::Ready(Some(2)));
        assert_eq!(poll_once(&queue, &mut cx), Poll::Ready(None));
    }

    /// Closing must wake a consumer that is parked on an empty queue, or the
    /// session would hang instead of shutting down.
    #[test]
    fn closing_an_empty_queue_wakes_the_consumer() {
        let queue: WakeQueue<u8> = WakeQueue::new();
        let flag = Arc::new(AtomicBool::new(false));
        let waker = flag_waker(flag.clone());
        let mut cx = Context::from_waker(&waker);

        assert!(poll_once(&queue, &mut cx).is_pending());
        queue.close();
        assert!(flag.load(Ordering::SeqCst), "close must wake the consumer");
        assert_eq!(poll_once(&queue, &mut cx), Poll::Ready(None));
    }

    /// Items pushed after a close have no reader, so they must be dropped rather
    /// than accumulated.
    #[test]
    fn a_push_after_close_is_discarded() {
        let queue: WakeQueue<u8> = WakeQueue::new();
        queue.close();
        queue.push(1);
        assert_eq!(queue.len(), 0, "a closed queue must not grow");
    }

    /// Two consumers may be parked at once — MoQ clones its session so several
    /// operations are pending simultaneously — and a single push must wake both.
    #[test]
    fn every_parked_consumer_is_woken() {
        let queue: WakeQueue<u8> = WakeQueue::new();
        let (a, b) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        // Bound, not inlined: a `Context` borrows its waker, and a waker built in
        // the same statement would be dropped at the end of it.
        let waker_a = flag_waker(a.clone());
        let waker_b = flag_waker(b.clone());
        let mut cx_a = Context::from_waker(&waker_a);
        let mut cx_b = Context::from_waker(&waker_b);

        assert!(poll_once(&queue, &mut cx_a).is_pending());
        assert!(poll_once(&queue, &mut cx_b).is_pending());

        queue.push(1);
        assert!(a.load(Ordering::SeqCst) && b.load(Ordering::SeqCst));
        // One item, two waiters: exactly one of them takes it.
        assert_eq!(poll_once(&queue, &mut cx_a), Poll::Ready(Some(1)));
        assert!(poll_once(&queue, &mut cx_b).is_pending());
    }
}
