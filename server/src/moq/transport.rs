//! A poll-based MoQ transport over the session's existing `wtransport::Connection`.
//!
//! # Why an adapter is needed at all
//!
//! `moq-net` is sans-I/O. Its only entry points — [`moq_net::Client::connect_lite`],
//! [`moq_net::Server::accept_lite`] — are generic over
//! [`web_transport_trait::poll::Session`], the half of the WebTransport surface
//! where the caller steps a state machine by hand. `wtransport` implements only
//! the async half, so something has to bridge them, and `wtransport` cannot be
//! bypassed either: [`wtransport::Connection::open_uni`] writes the WebTransport
//! capsule preamble that makes the stream a WebTransport stream at all, and its
//! `accept_uni` is keyed on the session id from the capsule header. A raw quinn
//! stream is not a WebTransport stream.
//!
//! So this is a real adapter, not a wrapper.
//!
//! # How much state it needs to retain
//!
//! [`web_transport_trait::poll`] allows a `poll_*` method to retain its own
//! progress between calls, and wtransport's connection-level operations are
//! futures. Each such future is boxed into `Option` and resumed on the next poll.
//!
//! Stream *data* I/O needs no retention at all, and deliberately has none:
//! [`wtransport::stream::SendStream::quic_stream_mut`] and its `RecvStream`
//! counterpart hand back the underlying quinn stream, which wtransport has
//! already stripped of the WebTransport session-id prefix, and quinn exposes
//! `poll_read`/`poll_write` directly. Those take the caller's buffer and honour
//! partial writes, which is precisely the contract the poll traits ask for — so
//! the adapter forwards rather than reimplements, and there is no self-referential
//! state to get wrong.
//!
//! # The one lie
//!
//! [`Session::protocol`] reports `"moq-lite-06"` no matter what wtransport
//! negotiated. This session is a webshooter WebTransport session that *also* runs
//! MoQ, not a MoQ session; there is no MoQ ALPN on the wire, and nothing
//! negotiates one. But `Server::accept_lite` selects the protocol driver purely
//! from this string and refuses everything it does not recognise, so reporting
//! the truth (`None`, or webshooter's own subprotocol) means MoQ never starts.
//! Reporting a fixed moq-lite ALPN is the only way to get a moq-lite driver on a
//! session that is shared with another protocol.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use bytes::Bytes;
use wtransport::Connection;
use wtransport::error::SendDatagramError;

use crate::moq::Inbox;

/// The stream pair a bidirectional WebTransport stream hands back.
type BiStreams = Result<
    (
        wtransport::stream::SendStream,
        wtransport::stream::RecvStream,
    ),
    Error,
>;

/// The ALPN reported to `moq-net`. See the module docs on why it is fixed.
///
/// `moq-lite-06` is chosen over the alternatives because it has a SETUP stream
/// (`has_setup_stream`) but still encodes every varint in one byte for the values
/// webshooter uses, which is what keeps the whole MoQ first-byte range inside
/// `0x00..=0x3F`.
pub const ALPN: &str = "moq-lite-06";

/// `H3_DATAGRAM_ERROR`, from `wtransport_proto::error::h3_error_codes`.
const H3_DATAGRAM_ERROR: u32 = 0x33;
/// `H3_STREAM_CREATION_ERROR`.
const H3_STREAM_CREATION_ERROR: u32 = 0x103;
/// `H3_MESSAGE_ERROR`.
const H3_MESSAGE_ERROR: u32 = 0x10e;

/// Everything this adapter can fail with.
///
/// The poll traits permit exactly one error type per session, so the distinctions
/// wtransport draws between a session failure and a stream failure have to be
/// flattened. What is *not* flattened is which of the trait's accessors reports
/// the code — MoQ reads [`Error::session_code`] to decide whether a close is a
/// protocol error worth reporting or a peer that simply went away, so the levels
/// have to survive even though the type cannot.
///
/// There is deliberately no session-scoped code variant. Every connection-level
/// failure this adapter sees (`accept_uni`, `accept_bi`, `closed`) means the same
/// thing — there is no session left to run on — and reporting one as a coded
/// protocol error would tell MoQ to log and retry something that cannot be
/// retried. Session closes are therefore always [`Error::Closed`], and the coded
/// variants are all stream-scoped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The connection is gone, or has been closed locally.
    Closed(String),
    /// A stream-level operation failed: a read, a write, a stop, or a refused
    /// open.
    Stream { code: u32, reason: String },
    /// A datagram could not be sent or received.
    Datagram { code: u32, reason: String },
}

impl Error {
    /// Narrow a QUIC stream error code to the 32-bit H3 code the poll traits carry.
    ///
    /// QUIC's are 62-bit; the H3 codes in play all fit in 32 bits, so this only
    /// ever truncates a code that was never one of ours to begin with. Saturating
    /// rather than wrapping keeps a diagnostic from reading as a small sensible
    /// number, which would be worse than one that plainly does not fit.
    fn h3_code(code: u64) -> u32 {
        u32::try_from(code).unwrap_or(H3_MESSAGE_ERROR)
    }

    /// A closed session has no error code: nothing failed, the session ended.
    pub(crate) fn closed(reason: impl Into<String>) -> Self {
        Error::Closed(reason.into())
    }

    /// The code to report as a *session* close, or `None` for a local close.
    pub fn session_code(&self) -> Option<u32> {
        match self {
            // A stream failure closes the session with its code so the peer can
            // see *which* stream failed; a datagram that would not send is
            // dropped rather than surfaced, so it never reaches here.
            Error::Stream { code, .. } => Some(*code),
            Error::Closed(_) | Error::Datagram { .. } => None,
        }
    }

    fn from_connection(err: wtransport::error::ConnectionError, what: &str) -> Self {
        Error::Closed(format!("{what}: {err}"))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Closed(reason) => write!(f, "session closed: {reason}"),
            Error::Stream { code, reason } | Error::Datagram { code, reason } => {
                write!(f, "{reason} (code {code:#x})")
            }
        }
    }
}

impl std::error::Error for Error {}

impl web_transport_trait::Error for Error {
    fn session_error(&self) -> Option<(u32, String)> {
        self.session_code().map(|code| (code, self.to_string()))
    }

    fn stream_error(&self) -> Option<u32> {
        match self {
            Error::Stream { code, .. } | Error::Datagram { code, .. } => Some(*code),
            _ => None,
        }
    }
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Every connection-level operation this session has in flight.
///
/// The poll traits permit a `poll_*` method to retain its own progress between
/// calls, and wtransport's connection-level operations are futures, so each such
/// operation lives here until it resolves. One struct rather than five fields so
/// there is exactly one thing to lock.
#[derive(Default)]
struct Slots {
    accept_uni: Option<BoxFuture<Result<wtransport::stream::RecvStream, Error>>>,
    accept_bi: Option<BoxFuture<BiStreams>>,
    open_uni: Option<BoxFuture<Result<wtransport::stream::SendStream, Error>>>,
    open_bi: Option<BoxFuture<BiStreams>>,
    closed: Option<BoxFuture<Error>>,
}

impl Slots {
    /// Whether any operation is currently in progress.
    fn busy(&self) -> bool {
        self.accept_uni.is_some()
            || self.accept_bi.is_some()
            || self.open_uni.is_some()
            || self.open_bi.is_some()
    }
}

/// The MoQ session's view of the connection.
///
/// Clone it to get a second handle with its own in-progress operations; see the
/// `Clone` impl for what that costs.
pub struct WtSession {
    conn: Arc<Connection>,
    inbox: Inbox,
    /// Behind a mutex rather than held plainly, for `Sync` and nothing else.
    ///
    /// `moq-net` shares this session across `Arc`s, so `WtSession` has to be
    /// `Sync`; a `Box<dyn Future + Send>` is `Send` but not `Sync`, and a plain
    /// field would make the whole thing neither. The lock is uncontended by
    /// construction — MoQ clones the session rather than sharing it, so one
    /// driver owns it — and is taken for the duration of a single `poll_*`, which
    /// never re-enters this type.
    slots: Mutex<Slots>,
}

impl WtSession {
    /// Build a MoQ handle over `connection`, reading from `inbox`.
    ///
    /// The webshooter half of the mux is deliberately *not* built here: an
    /// [`AppSide`] is only useful to whoever feeds it, and inventing one here
    /// would hand the caller a queue the mux pump never writes to. See
    /// [`crate::moq::attach`], which owns that side.
    pub(crate) fn attach(connection: Arc<Connection>, inbox: Inbox) -> Self {
        Self {
            conn: connection,
            inbox,
            slots: Mutex::new(Slots::default()),
        }
    }

    /// Drive `fut` to completion across repeated polls, returning `Pending` while
    /// no such operation is in flight.
    ///
    /// The `is_some` guard is what makes this safe to call with an empty slot:
    /// an absent operation is reported as `Pending` rather than as a completion,
    /// so the caller cannot mistake "not started" for "finished".
    fn poll_slot<T>(slot: &mut Option<BoxFuture<T>>, cx: &mut Context<'_>) -> Poll<T> {
        let Some(fut) = slot.as_mut() else {
            return Poll::Pending;
        };
        let result = fut.as_mut().poll(cx);
        if result.is_ready() {
            *slot = None;
        }
        result
    }

    fn slots(&self) -> MutexGuard<'_, Slots> {
        // The only other user of this lock is `Clone`, which takes it briefly and
        // holds nothing across an await. There is no poisoning risk to speak of:
        // nothing in the critical section can panic.
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Clone for WtSession {
    fn clone(&self) -> Self {
        // Each handle carries its own in-progress operation, as the poll contract
        // requires: a `poll_open_uni` that reserved stream credit belongs to the
        // handle that started it. Cloning therefore *abandons* whatever was in
        // flight rather than duplicating it.
        //
        // MoQ clones at the start of an operation, not partway through one, so
        // this should never actually drop anything. The alternative — sharing one
        // set of slots between clones — is worse: two handles could then resume
        // the same `accept_uni` and lose a stream between them.
        if self.slots().busy() {
            log::debug!("MoQ session cloned with an operation in flight; abandoning it");
        }
        Self {
            conn: self.conn.clone(),
            inbox: self.inbox.clone(),
            slots: Mutex::new(Slots::default()),
        }
    }
}

impl web_transport_trait::poll::Session for WtSession {
    type SendStream = WtSendStream;
    type RecvStream = WtRecvStream;
    type Error = Error;

    fn poll_accept_uni(&mut self, cx: &mut Context<'_>) -> Poll<Result<Self::RecvStream, Error>> {
        // Deliberately *not* the connection's own `accept_uni`. The mux pump owns
        // that, and everything it recognises as MoQ arrives here instead.
        match self.inbox.unistreams.poll_pop(cx) {
            Poll::Ready(Some(stream)) => Poll::Ready(Ok(stream)),
            Poll::Ready(None) => Poll::Ready(Err(Error::closed("mux pump stopped"))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_accept_bi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<web_transport_trait::poll::BiStreams<Self>, Error>> {
        // Bidirectional streams are not demultiplexed: webshooter never opens
        // one, so there is nothing to collide with. MoQ-lite uses them for the
        // control stream.
        let mut slots = self.slots();
        if slots.accept_bi.is_none() {
            let conn = self.conn.clone();
            slots.accept_bi = Some(Box::pin(async move {
                conn.accept_bi()
                    .await
                    .map_err(|e| Error::from_connection(e, "accepting a bidirectional stream"))
            }));
        }
        match Self::poll_slot(&mut slots.accept_bi, cx) {
            Poll::Ready(Ok((send, recv))) => {
                Poll::Ready(Ok((WtSendStream { inner: send }, WtRecvStream::new(recv))))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_open_uni(&mut self, cx: &mut Context<'_>) -> Poll<Result<Self::SendStream, Error>> {
        let mut slots = self.slots();
        if slots.open_uni.is_none() {
            let conn = self.conn.clone();
            slots.open_uni = Some(Box::pin(async move {
                // Two awaits, and both are load-bearing: the first reserves the
                // stream, the second completes the WebTransport handshake that
                // makes it usable. The second one is where a multi-megabyte
                // group's first write waits for flow-control credit.
                let opening = conn
                    .open_uni()
                    .await
                    .map_err(|e| Error::from_connection(e, "opening a unidirectional stream"))?;
                opening.await.map_err(|e| Error::Stream {
                    code: H3_STREAM_CREATION_ERROR,
                    reason: e.to_string(),
                })
            }));
        }
        match Self::poll_slot(&mut slots.open_uni, cx) {
            Poll::Ready(Ok(stream)) => Poll::Ready(Ok(WtSendStream { inner: stream })),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_open_bi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<web_transport_trait::poll::BiStreams<Self>, Error>> {
        let mut slots = self.slots();
        if slots.open_bi.is_none() {
            let conn = self.conn.clone();
            slots.open_bi = Some(Box::pin(async move {
                let opening = conn
                    .open_bi()
                    .await
                    .map_err(|e| Error::from_connection(e, "opening a bidirectional stream"))?;
                opening.await.map_err(|e| Error::Stream {
                    code: H3_STREAM_CREATION_ERROR,
                    reason: e.to_string(),
                })
            }));
        }
        match Self::poll_slot(&mut slots.open_bi, cx) {
            Poll::Ready(Ok((send, recv))) => {
                Poll::Ready(Ok((WtSendStream { inner: send }, WtRecvStream::new(recv))))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_send_datagram(
        &mut self,
        _cx: &mut Context<'_>,
        payload: &[u8],
    ) -> Poll<Result<(), Error>> {
        // Synchronous: quinn either has datagram room or it does not, and
        // reports which. A "no room" answer is reported as success, matching
        // `transport::poll::Session::send_datagram`'s best-effort contract —
        // webshooter carries media on reliable streams, so the only datagrams
        // here are MoQ's own probe and announce traffic, which are allowed to
        // be dropped.
        match self.conn.send_datagram(payload) {
            Ok(()) | Err(SendDatagramError::TooLarge) => Poll::Ready(Ok(())),
            Err(e @ SendDatagramError::UnsupportedByPeer) => Poll::Ready(Err(Error::Datagram {
                code: H3_DATAGRAM_ERROR,
                reason: e.to_string(),
            })),
            Err(e @ SendDatagramError::NotConnected) => {
                Poll::Ready(Err(Error::closed(e.to_string())))
            }
        }
    }

    fn poll_recv_datagram(&mut self, cx: &mut Context<'_>) -> Poll<Result<Bytes, Error>> {
        match self.inbox.datagrams.poll_pop(cx) {
            Poll::Ready(Some(dgram)) => Poll::Ready(Ok(dgram)),
            Poll::Ready(None) => Poll::Ready(Err(Error::closed("mux pump stopped"))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn max_datagram_size(&self) -> usize {
        self.conn.max_datagram_size().unwrap_or(0)
    }

    fn protocol(&self) -> Option<&str> {
        Some(ALPN)
    }

    fn close(&mut self, code: u32, reason: &str) {
        self.conn
            .close(wtransport::VarInt::from_u32(code), reason.as_bytes());
    }

    fn poll_closed(&mut self, cx: &mut Context<'_>) -> Poll<Error> {
        let mut slots = self.slots();
        if slots.closed.is_none() {
            let conn = self.conn.clone();
            slots.closed = Some(Box::pin(async move {
                conn.closed().await;
                Error::closed("connection closed")
            }));
        }
        match Self::poll_slot(&mut slots.closed, cx) {
            Poll::Ready(e) => Poll::Ready(e),
            Poll::Pending => Poll::Pending,
        }
    }

    fn stats(&self) -> impl web_transport_trait::Stats {
        web_transport_trait::StatsUnavailable
    }
}

/// An outgoing stream.
///
/// A thin wrapper: every method forwards to the quinn stream underneath, which
/// wtransport has already made a WebTransport stream.
pub struct WtSendStream {
    inner: wtransport::stream::SendStream,
}

impl web_transport_trait::poll::SendStream for WtSendStream {
    type Error = Error;

    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Error>> {
        // quinn's `poll_write` writes a *prefix* and reports how much, which is
        // exactly the partial-write contract; nothing is consumed from `buf`
        // unless it returns `Ready`. WebTransport adds no framing to the payload,
        // so there is nothing to account for on top.
        match std::pin::Pin::new(&mut *self.inner.quic_stream_mut()).poll_write(cx, buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
            Poll::Ready(Err(quinn::WriteError::Stopped(code))) => Poll::Ready(Err(Error::Stream {
                code: Error::h3_code(code.into_inner()),
                reason: "peer stopped the stream".into(),
            })),
            Poll::Ready(Err(quinn::WriteError::ClosedStream)) => {
                Poll::Ready(Err(Error::closed("write to a closed stream")))
            }
            // A lost connection and a rejected early connection are both just
            // "the transport is gone" as far as MoQ is concerned: neither leaves
            // this stream usable, and neither is a stream-scoped condition that
            // `stream_error` could describe honestly.
            Poll::Ready(Err(e)) => Poll::Ready(Err(Error::closed(e.to_string()))),
        }
    }

    fn set_priority(&mut self, order: u8) {
        // wtransport's setter is fallible on a closed stream, and there is
        // nothing useful to do about that here: the priority of a stream nobody
        // will read is the priority of nothing. The session-level error, if the
        // stream really is gone, arrives on its own operations.
        self.inner.set_priority(order as i32);
    }

    fn finish(&mut self) -> Result<(), Error> {
        self.inner
            .quic_stream_mut()
            .finish()
            .map_err(|_| Error::closed("finishing a closed stream"))
    }

    fn reset(&mut self, code: u32) {
        let _ = self.inner.reset(wtransport::VarInt::from_u32(code));
    }

    fn poll_closed(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        // quinn's `stopped` future owns everything it needs and is created fresh
        // per poll, so there is nothing to retain here: pin it on the stack, poll
        // it once, and let it go. The cost is rebuilding it each time, which for
        // a future that registers one waker is not worth retaining a slot for.
        let mut stopped = std::pin::pin!(self.inner.quic_stream().stopped());
        match stopped.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(Error::closed(e.to_string()))),
        }
    }
}

/// An incoming stream, plus the first byte the mux already read to classify it.
pub struct WtRecvStream {
    inner: wtransport::stream::RecvStream,
    /// Set by the mux: the byte it consumed to decide which protocol owns this
    /// stream, still owed to whoever ends up reading it.
    peeked: Option<u8>,
}

impl WtRecvStream {
    pub(crate) fn new(inner: wtransport::stream::RecvStream) -> Self {
        Self {
            inner,
            peeked: None,
        }
    }

    /// Wrap a stream whose first byte the mux already read to classify it.
    ///
    /// The byte is owed back to whoever reads next: it was consumed for routing,
    /// not for reading, and dropping it would shift the whole message by one byte.
    pub(crate) fn with_first(inner: wtransport::stream::RecvStream, first: u8) -> Self {
        Self {
            inner,
            peeked: Some(first),
        }
    }

    /// Read a stream to its end, for the webshooter side of the mux.
    ///
    /// Its control messages are small and arrive whole, so buffering one is free,
    /// whereas a MoQ group is not — that side reads incrementally through
    /// [`poll_read`](web_transport_trait::poll::RecvStream::poll_read).
    pub(crate) async fn read_to_end(mut self) -> Result<Vec<u8>, Error> {
        // The buffer belongs to the *future*, not the closure: `poll_fn` takes an
        // `FnMut`, which cannot give its captures away, and a closure that could
        // would only ever run once anyway. So the closure reports "done" and the
        // bytes are handed over after the await.
        let mut out = Vec::new();
        std::future::poll_fn(|cx| {
            let mut chunk = [0u8; 8 * 1024];
            loop {
                match web_transport_trait::poll::RecvStream::poll_read(&mut self, cx, &mut chunk) {
                    // A destination with room that yields nothing is the end of
                    // the stream, not a stall.
                    Poll::Ready(Ok(Some(0))) | Poll::Ready(Ok(None)) => {
                        return Poll::Ready(Ok(()));
                    }
                    Poll::Ready(Ok(Some(n))) => out.extend_from_slice(&chunk[..n]),
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                }
            }
        })
        .await?;
        Ok(out)
    }
}

impl web_transport_trait::poll::RecvStream for WtRecvStream {
    type Error = Error;

    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        dst: &mut [u8],
    ) -> Poll<Result<Option<usize>, Error>> {
        // An empty destination is not the end of the stream, so the peeked byte
        // must survive it — otherwise a caller with a full buffer would make the
        // message look truncated.
        if let Some(byte) = self.peeked {
            if dst.is_empty() {
                return Poll::Ready(Ok(Some(0)));
            }
            self.peeked = None;
            dst[0] = byte;
            return Poll::Ready(Ok(Some(1)));
        }
        match std::pin::Pin::new(&mut *self.inner.quic_stream_mut()).poll_read(cx, dst) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(0)) => Poll::Ready(Ok(None)),
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(Some(n))),
            Poll::Ready(Err(quinn::ReadError::Reset(code))) => Poll::Ready(Err(Error::Stream {
                code: Error::h3_code(code.into_inner()),
                reason: "peer reset the stream".into(),
            })),
            Poll::Ready(Err(quinn::ReadError::ClosedStream)) => {
                Poll::Ready(Err(Error::closed("read from a closed stream")))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(Error::Stream {
                code: H3_MESSAGE_ERROR,
                reason: e.to_string(),
            })),
        }
    }

    fn stop(&mut self, code: u32) {
        let _ = self
            .inner
            .quic_stream_mut()
            .stop(quinn::VarInt::from_u32(code));
    }

    fn poll_closed(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        // A receive stream is closed once it is finished-and-drained or reset.
        // A one-byte buffer is enough to ask, and the peeked byte is untouched
        // because the mux has not classified this stream yet in the normal path.
        let mut byte = [0u8; 1];
        match std::pin::Pin::new(&mut *self.inner.quic_stream_mut()).poll_read(cx, &mut byte) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(0)) => Poll::Ready(Ok(())),
            Poll::Ready(Ok(_)) => Poll::Pending,
            Poll::Ready(Err(_)) => Poll::Ready(Ok(())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session has to cross a thread boundary, and it is not automatic.
    ///
    /// `moq-net` shares the session behind `Arc`s, so a `!Sync` session makes the
    /// whole driver `!Send`, and the driver loop — which has to be a task of its
    /// own, polling the transport and the content side on one loop — could not be
    /// spawned at all. The retained connection-level futures are exactly why that
    /// is in doubt: `Box<dyn Future + Send>` is `Send` but not `Sync`, so each one
    /// has to sit behind a lock. This pins the result of that.
    #[test]
    fn the_session_and_its_parts_cross_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WtSession>();
        assert_send_sync::<Error>();
        assert_send_sync::<WtSendStream>();
        assert_send_sync::<WtRecvStream>();
        assert_send_sync::<Inbox>();
    }
}
