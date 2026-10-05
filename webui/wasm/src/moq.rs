//! MoQ sharing the browser's single WebTransport session with webshooter's own
//! control protocol.
//!
//! # Why an adapter is needed at all
//!
//! `moq-net` is sans-I/O. Its only entry points — [`moq_net::Client::connect_lite`]
//! here, and `Server::accept_lite` on the server — are generic over
//! [`web_transport_trait::poll::Session`], the half of the WebTransport surface
//! where the caller steps a state machine by hand. `web_sys::WebTransport`
//! dispatches everything through JS promises and streams, so something has to
//! bridge them. This is a real adapter, not a wrapper.
//!
//! # Who owns which read
//!
//! A WebTransport has exactly one reader per source: the datagrams readable and
//! the incoming-unistream readable. Both sides of the mux want one, so neither
//! gets direct access — the pumps in [`attach`] own both reads and hand each
//! message to one of four queues: MoQ's pair and the app's pair. MoQ drains its
//! pair through [`BrowserSession`], which is where the poll traits are
//! satisfied; the app side drains its pair through [`AppSide`].
//!
//! # The same lie as the server
//!
//! [`BrowserSession::protocol`] reports `"moq-lite-06"` no matter what the
//! WebTransport subprotocol actually negotiated. It is the only string that
//! makes `Client::connect_lite` pick the moq-lite driver; the wire genuinely is
//! moq-lite-06, only the browser's subprotocol field is not.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use shared::mux::is_moq_byte;
use shared::wake_queue::WakeQueue;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    ReadableStream, ReadableStreamDefaultReader, WebTransport, WebTransportDatagramDuplexStream,
    WritableStreamDefaultWriter,
};

/// The ALPN reported to `moq-net`. See the module docs on why it is fixed.
pub(crate) const ALPN: &str = "moq-lite-06";

type BoxFuture<T> = Pin<Box<dyn Future<Output = T>>>;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

type ErrorResult<T> = Result<T, Error>;

/// Everything this adapter can fail with.
///
/// One type per session, because the poll traits permit exactly one error type
/// per session — the distinctions between a session failure and a stream one
/// have to be flattened. What is *not* flattened is which of the trait's
/// accessors reports the code: the driver records a failure on the stream or on
/// the session depending on which one the error code names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Error {
    /// The connection is gone, or has been closed locally.
    Closed(String),
    /// A stream-level operation failed: a read, a write, a stop, or a refused
    /// open. Its code documents the shape in which it is reported for recovery
    /// text: the session aggregates streams, never their semantic error class.
    Stream { code: u32, reason: String },
}

impl Error {
    fn closed(reason: impl Into<String>) -> Self {
        Error::Closed(reason.into())
    }

    /// The reason carried by the most JS-flavoured error surface we can get.
    fn from_js(err: JsValue) -> String {
        err.as_string()
            .or_else(|| {
                js_sys::Reflect::get(&err, &JsValue::from_str("message"))
                    .ok()
                    .and_then(|msg| msg.as_string())
            })
            .unwrap_or_else(|| "unknown JS error".to_string())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Closed(reason) => write!(f, "session closed: {reason}"),
            Error::Stream { code, reason } => write!(f, "{reason} (code {code:#x})"),
        }
    }
}

impl std::error::Error for Error {}

impl moq_net::web_transport_trait::Error for Error {
    fn session_error(&self) -> Option<(u32, String)> {
        match self {
            // A non-closed session error always names the stream the peer
            // should record: that is what a stream error code is for.
            Error::Stream { code, .. } => Some((*code, self.to_string())),
            Error::Closed(_) => None,
        }
    }

    fn stream_error(&self) -> Option<u32> {
        match self {
            Error::Stream { code, .. } => Some(*code),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Partitioned queues: what the mux routes where
// ---------------------------------------------------------------------------

/// The MoQ half of the mux.
#[derive(Clone, Default)]
pub(crate) struct Inbox {
    unistreams: WakeQueue<MoQRecv>,
    datagrams: WakeQueue<Bytes>,
}

impl Inbox {
    /// Announce that nothing more will arrive. Closing is one-way and wakes
    /// every consumer parked on the queues, which is what tells the MoQ driver
    /// the session is over instead of leaving it waiting for a stream that is
    /// never coming.
    fn close(&self) {
        self.unistreams.close();
        self.datagrams.close();
    }
}

/// The webshooter half of the mux: messages the *server* sent, on either
/// carrier.
#[derive(Clone, Default)]
pub(crate) struct AppSide {
    datagrams: WakeQueue<Bytes>,
    unistreams: WakeQueue<Vec<u8>>,
}

impl AppSide {
    /// Announce that nothing more will arrive, so [`AppSide::next_control`]
    /// resolves `None` rather than parking forever.
    fn close(&self) {
        self.datagrams.close();
        self.unistreams.close();
    }
}

impl AppSide {
    /// The next control message from the server, whatever carrier it arrived on.
    ///
    /// Polls both queues on the given waker: when either receives a message the
    /// self-framing `ServerDatagram` it carries out is already complete — a
    /// datagram in one shot, or an unistream already drained to its end by the
    /// pump. Both queues have nominally responded to our waker via their own
    /// waiters, and the `WakeQueue::will_wake` dedup keeps us from stacking
    /// wakers per tick.
    pub(crate) async fn next_control(&self) -> Option<Vec<u8>> {
        // Which of the two carriers is drained-and-closed is remembered across
        // calls, so that one carrier running dry mid-session does not report the
        // session's end while the other still has messages queued. A queue only
        // reports `Ready(None)` once it is both closed and drained, so the flag
        // set here cannot strand a message.
        let mut datagrams_done = false;
        let mut unistreams_done = false;
        std::future::poll_fn(move |cx| {
            if !datagrams_done {
                match self.datagrams.poll_pop(cx) {
                    Poll::Ready(Some(datagram)) => return Poll::Ready(Some(datagram.to_vec())),
                    Poll::Ready(None) => datagrams_done = true,
                    Poll::Pending => {}
                }
            }
            if !unistreams_done {
                match self.unistreams.poll_pop(cx) {
                    Poll::Ready(Some(message)) => return Poll::Ready(Some(message)),
                    Poll::Ready(None) => unistreams_done = true,
                    Poll::Pending => {}
                }
            }
            if datagrams_done && unistreams_done {
                return Poll::Ready(None);
            }
            Poll::Pending
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// MoQRecv / MoQSend: the browser's ReadableStream/WritableStream under the
// poll traits. Each is async-ified one promise at a time: one read promise or
// one write promise is kept between polls, and it is not dropped unpolled.
// ---------------------------------------------------------------------------

/// An incoming stream the mux has classified as MoQ's.
///
/// There is no way to demand "just N bytes" at JS stream level either:
/// `ReadableStreamDefaultReader::read` yields one chunk and nothing more
/// precisely-shaped. The chunk that named this stream — delivered before the
/// caller got to `poll_read` — is held as `burst`, and a next read is only
/// issued once it has been completely drained, so no resolved chunk is lost
/// between polls when the caller only wanted part of it.
pub(crate) struct MoQRecv {
    reader: ReadableStreamDefaultReader,
    in_flight: Option<BoxFuture<Result<JsValue, JsValue>>>,
    burst: Vec<u8>,
    burst_head: usize,
    done: bool,
    /// The reader's own `closed()` promise, built on first `poll_closed` and kept
    /// until it resolves. Unlike a read it says "this stream is finished" without
    /// consuming a byte, which is what the poll trait asks for and what quinn's
    /// `stopped()` reports on the server side.
    closed: Option<BoxFuture<Result<JsValue, JsValue>>>,
}

impl MoQRecv {
    fn with_initial_chunk(reader: ReadableStreamDefaultReader, initial: Vec<u8>) -> Self {
        Self {
            reader,
            in_flight: None,
            burst: initial,
            burst_head: 0,
            done: false,
            closed: None,
        }
    }

    fn drain_burst(&mut self, dst: &mut [u8]) -> Option<usize> {
        let avail = self.burst.len() - self.burst_head;
        if avail == 0 {
            return None;
        }
        let n = avail.min(dst.len());
        dst[..n].copy_from_slice(&self.burst[self.burst_head..self.burst_head + n]);
        self.burst_head += n;
        if self.burst_head == self.burst.len() {
            self.burst.clear();
            self.burst_head = 0;
        }
        Some(n)
    }
}

impl moq_net::web_transport_trait::poll::RecvStream for MoQRecv {
    type Error = Error;

    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        dst: &mut [u8],
    ) -> Poll<Result<Option<usize>, Error>> {
        if let Some(n) = self.drain_burst(dst) {
            return Poll::Ready(Ok(Some(n)));
        }
        if self.done {
            return Poll::Ready(Ok(None));
        }
        if self.in_flight.is_none() {
            let promise = self.reader.read();
            self.in_flight = Some(Box::pin(async move { JsFuture::from(promise).await }));
        }
        let fut = match self.in_flight.as_mut() {
            Some(fut) => fut,
            None => unreachable!("set above"),
        };
        match fut.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => {
                self.in_flight = None;
                Poll::Ready(Err(Error::Stream {
                    code: 0x10e,
                    reason: Error::from_js(err),
                }))
            }
            Poll::Ready(Ok(chunk)) => {
                self.in_flight = None;
                let done = js_sys::Reflect::get(&chunk, &JsValue::from_str("done"))
                    .ok()
                    .and_then(|d| d.as_bool())
                    .unwrap_or(false);
                if done {
                    // End of stream: everything the publisher wrote has been read
                    // by now, and `burst` is empty because the top of this call
                    // drains it first. Any bytes sitting in `burst` would have
                    // been drained at the top of this very call.
                    self.done = true;
                    return Poll::Ready(Ok(None));
                }
                let value = match js_sys::Reflect::get(&chunk, &JsValue::from_str("value")) {
                    Ok(value) if !value.is_undefined() && !value.is_null() => value,
                    _ => {
                        return Poll::Ready(Err(Error::Stream {
                            code: 0x10e,
                            reason: "stream chunk missing value".into(),
                        }));
                    }
                };
                let bytes = js_sys::Uint8Array::new(&value);
                let mut buf = vec![0u8; bytes.length() as usize];
                bytes.copy_to(&mut buf[..]);
                self.burst.extend_from_slice(&buf);
                self.burst_head = 0;
                match self.drain_burst(dst) {
                    Some(n) => Poll::Ready(Ok(Some(n))),
                    // A chunk of no bytes on a stream that is not finished. Not
                    // end of stream — that would truncate the group — and not
                    // `Pending` either: the promise it came from has been
                    // consumed, so nothing is left to wake us. Report no progress
                    // and let the caller ask again, which starts a fresh read.
                    None => Poll::Ready(Ok(Some(0))),
                }
            }
        }
    }

    fn stop(&mut self, _code: u32) {
        // Tell the reader to abandon it: subsequent pending reads reject,
        // surfacing as the stream-error the lite driver drops the group for.
        let _ = self.reader.cancel();
    }

    fn poll_closed(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        // The reader's `closed()` promise, rather than the `done` flag: it
        // settles when the stream itself is finished — including when it is
        // reset, which arrives as a rejection — and it consumes no bytes, which
        // a read-based check would.
        if self.closed.is_none() {
            let promise = self.reader.closed();
            self.closed = Some(Box::pin(async move { JsFuture::from(promise).await }));
        }
        match self.closed.as_mut().expect("set above").as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(err)) => Poll::Ready(Err(Error::Stream {
                code: 0x10e,
                reason: Error::from_js(err),
            })),
        }
    }
}

/// An outgoing stream the mux has classified as MoQ's.
///
/// One write promise between polls: calling `write` twice on the same
/// underlying stream without awaiting both is an ordering error in JS streams —
/// the chunks may resolve out of order. Each `poll_write` starts its write only
/// once the previous one has resolved, holding the waker the same way
/// `poll_read` does.
pub(crate) struct MoQSend {
    writer: WritableStreamDefaultWriter,
    in_flight_write: Option<BoxFuture<Result<JsValue, JsValue>>>,
    finished: bool,
    in_flight_close: Option<BoxFuture<Result<JsValue, JsValue>>>,
}

impl MoQSend {
    fn over(writer: WritableStreamDefaultWriter) -> Self {
        Self {
            writer,
            in_flight_write: None,
            finished: false,
            in_flight_close: None,
        }
    }
}

impl moq_net::web_transport_trait::poll::SendStream for MoQSend {
    type Error = Error;

    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Error>> {
        if self.finished {
            return Poll::Ready(Err(Error::closed("write to a finished stream")));
        }
        if self.in_flight_write.is_none() {
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            let chunk = js_sys::Uint8Array::from(buf);
            let promise = self.writer.write_with_chunk(&chunk);
            self.in_flight_write = Some(Box::pin(async move { JsFuture::from(promise).await }));
        }
        let fut = match self.in_flight_write.as_mut() {
            Some(fut) => fut,
            None => unreachable!("set above"),
        };
        match fut.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(_)) => {
                let n = buf.len();
                self.in_flight_write = None;
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(err)) => {
                self.in_flight_write = None;
                Poll::Ready(Err(Error::Stream {
                    code: 0x10e,
                    reason: Error::from_js(err),
                }))
            }
        }
    }

    fn set_priority(&mut self, _order: u8) {
        // WebTransport streams are not reorderable — the browser queues writes in
        // call order — so there is nothing for a sender-priority hint to change.
        // The trait takes a number rather than an option, so it is accepted and
        // dropped rather than refused.
    }

    fn finish(&mut self) -> Result<(), Error> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        // `close()` orders *after* every pending write on the same writer —
        // which is exactly why their promise was in one slot: no write call is
        // still pending now. The close promise gets polled from `poll_closed`.
        let promise = self.writer.close();
        self.in_flight_close = Some(Box::pin(async move { JsFuture::from(promise).await }));
        Ok(())
    }

    fn reset(&mut self, _code: u32) {
        // The JS stream API has no reset with a code: abort discards. The lite
        // driver only reaches for it on cancellation, which abort matches in
        // spirit: no more data travels, the peer sees EOF.
        let _ = self.writer.abort();
    }

    fn poll_closed(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        // The lite driver flushes only once: `finish()` starts the close, this
        // polls it to resolve. `Ready` means the peer's reader saw EOF.
        match self.in_flight_close.as_mut() {
            None => Poll::Pending,
            Some(fut) => match fut.as_mut().poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
                Poll::Ready(Err(err)) => Poll::Ready(Err(Error::Stream {
                    code: 0x10e,
                    reason: Error::from_js(err),
                })),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// BrowserSession: the browser-side poll::Session
// ---------------------------------------------------------------------------

/// The MoQ session's view of the browser WebTransport.
///
/// Clone it to get a second handle with its own in-progress operations; the
/// clone drops any in-flight promise, which is fine because `moq-net` clones at
/// the start of an operation, not partway through one.
pub(crate) struct BrowserSession {
    wt: WebTransport,
    inbox: Inbox,
    /// The one writer over the datagrams' `WritableStream`, shared with the
    /// webshooter control path (see [`attach`]). A `WritableStream` admits one
    /// writer at a time and cannot be asked for a second, so this is a handle
    /// on the *same* writer rather than a writer of our own.
    datagrams: WritableStreamDefaultWriter,
    /// The datagram write awaiting its promise, if any.
    in_flight_datagram: Option<BoxFuture<Result<JsValue, JsValue>>>,
    open_uni: Option<BoxFuture<ErrorResult<MoQSend>>>,
    open_bi: Option<BoxFuture<ErrorResult<(MoQSend, MoQRecv)>>>,
    closed: Option<BoxFuture<Error>>,
}

impl Clone for BrowserSession {
    fn clone(&self) -> Self {
        Self {
            wt: self.wt.clone(),
            inbox: self.inbox.clone(),
            datagrams: self.datagrams.clone(),
            in_flight_datagram: None,
            open_uni: None,
            open_bi: None,
            closed: None,
        }
    }
}

impl BrowserSession {
    fn over(wt: WebTransport, inbox: Inbox, datagrams: WritableStreamDefaultWriter) -> Self {
        Self {
            wt,
            inbox,
            datagrams,
            in_flight_datagram: None,
            open_uni: None,
            open_bi: None,
            closed: None,
        }
    }

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
}

impl moq_net::web_transport_trait::poll::Session for BrowserSession {
    type SendStream = MoQSend;
    type RecvStream = MoQRecv;
    type Error = Error;

    fn poll_accept_uni(&mut self, cx: &mut Context<'_>) -> Poll<ErrorResult<MoQRecv>> {
        // Deliberately *not* the incoming-unistream pump. The pump owns that
        // reader, and everything it recognises as MoQ arrives here instead.
        match self.inbox.unistreams.poll_pop(cx) {
            Poll::Ready(Some(stream)) => Poll::Ready(Ok(stream)),
            Poll::Ready(None) => Poll::Ready(Err(Error::closed("mux pump stopped"))),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Browser sessions never accept peer-created bidi streams in our design:
    /// the lite handshake runs on a uni SETUP stream. An accept that is
    /// permanently pending is what a client should return when it has nothing
    /// to read there.
    fn poll_accept_bi(
        &mut self,
        _cx: &mut Context<'_>,
    ) -> Poll<ErrorResult<moq_net::web_transport_trait::poll::BiStreams<Self>>> {
        Poll::Pending
    }

    fn poll_open_uni(&mut self, cx: &mut Context<'_>) -> Poll<ErrorResult<MoQSend>> {
        if self.open_uni.is_none() {
            let wt = self.wt.clone();
            self.open_uni = Some(Box::pin(async move {
                let opening = JsFuture::from(wt.create_unidirectional_stream()).await;
                // Skip the `WebTransportSendStream` class: a checked `dyn_into` for
                // that type does not allBrowser ways match the concrete object a
                // WebTransport in Chrome returns here (an unstable class that is not
                // necessarily the one web-sys watched). Treating it as the
                // WritableStream it implements behaves the same for everything the
                // adapter needs from it.
                let writable: web_sys::WritableStream = match opening {
                    Ok(s) => s.dyn_into::<web_sys::WritableStream>().map_err(|_| {
                        Error::Stream {
                            code: 0x103,
                            reason: "createUnidirectionalStream yielded no WritableStream".into(),
                        }
                    })?,
                    Err(err) => {
                        return Err(Error::Stream {
                            code: 0x103,
                            reason: Error::from_js(err),
                        });
                    }
                };
                let writer = writable.get_writer().map_err(|err| Error::Stream {
                    code: 0x103,
                    reason: Error::from_js(err),
                })?;
                Ok(MoQSend::over(writer))
            }));
        }
        match Self::poll_slot(&mut self.open_uni, cx) {
            Poll::Ready(Ok(stream)) => {
                ::log::info!("MoQ client: uni stream opened");
                Poll::Ready(Ok(stream))
            }
            Poll::Ready(Err(e)) => {
                ::log::warn!("MoQ client: opening a uni stream failed: {e}");
                Poll::Ready(Err(e))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_open_bi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ErrorResult<moq_net::web_transport_trait::poll::BiStreams<Self>>> {
        if self.open_bi.is_none() {
            let wt = self.wt.clone();
            self.open_bi = Some(Box::pin(async move {
                let opening = JsFuture::from(wt.create_bidirectional_stream()).await;
                let stream = match opening {
                    Ok(s) => s
                        .dyn_into::<web_sys::WebTransportBidirectionalStream>()
                        .map_err(|_| Error::Stream {
                            code: 0x103,
                            reason: "could not read bidirectional stream".into(),
                        })?,
                    Err(err) => {
                        return Err(Error::Stream {
                            code: 0x103,
                            reason: Error::from_js(err),
                        });
                    }
                };
                let readable: ReadableStream = stream.readable().unchecked_into();
                let reader = match readable
                    .get_reader()
                    .dyn_into::<ReadableStreamDefaultReader>()
                {
                    Ok(reader) => reader,
                    Err(_) => {
                        return Err(Error::Stream {
                            code: 0x103,
                            reason: "could not read incoming half".into(),
                        });
                    }
                };
                // The `writable()` side is a `WebTransportSendStream`, which
                // extends `WritableStream`; upcast and let the writer factory
                // come from the parent.
                let writable: web_sys::WritableStream = stream.writable().unchecked_into();
                let writer = writable.get_writer().map_err(|err| Error::Stream {
                    code: 0x103,
                    reason: Error::from_js(err),
                })?;
                Ok((
                    MoQSend::over(writer),
                    MoQRecv::with_initial_chunk(reader, Vec::new()),
                ))
            }));
        }
        match Self::poll_slot(&mut self.open_bi, cx) {
            Poll::Ready(Ok((send, recv))) => Poll::Ready(Ok((send, recv))),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Write a datagram, or report that the shared writer is busy.
    ///
    /// The writer is the app's, so this is one ordered queue serving both
    /// protocols' datagrams rather than two writers racing for one stream. A
    /// write the app issues while this one is in flight queues behind it:
    /// `write_with_chunk` promises resolve in call order, so neither side sees
    /// its own datagrams reordered.
    fn poll_send_datagram(
        &mut self,
        cx: &mut Context<'_>,
        payload: &[u8],
    ) -> Poll<Result<(), Error>> {
        if self.in_flight_datagram.is_none() {
            let chunk = js_sys::Uint8Array::from(payload);
            let promise = self.datagrams.write_with_chunk(&chunk);
            self.in_flight_datagram = Some(Box::pin(async move { JsFuture::from(promise).await }));
        }
        match Self::poll_slot(&mut self.in_flight_datagram, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(err)) => Poll::Ready(Err(Error::closed(format!(
                "sending a datagram: {}",
                Error::from_js(err)
            )))),
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
        // Spec discovery: from `datagrams().maxDatagramSize`. Chrome caps it;
        // the MoQ caller consults it before each write.
        self.wt.datagrams().max_datagram_size() as usize
    }

    /// The fixed lie, in one place: see [`ALPN`].
    fn protocol(&self) -> Option<&str> {
        Some(ALPN)
    }

    fn close(&mut self, code: u32, reason: &str) {
        // The web API refuses some codes outright; the reason is capped. We
        // truncate reason to keep the call from throwing, but never rewrite
        // the code: the alternative would silently disagree with the server.
        let mut info = web_sys::WebTransportCloseInfo::new();
        info.close_code(code);
        info.reason(if reason.len() <= 1024 {
            reason
        } else {
            &reason[..1024]
        });
        self.wt.close_with_close_info(&info);
    }

    fn poll_closed(&mut self, cx: &mut Context<'_>) -> Poll<Error> {
        if self.closed.is_none() {
            let wt = self.wt.clone();
            self.closed = Some(Box::pin(async move {
                let _ = JsFuture::from(wt.closed()).await;
                Error::closed("connection closed")
            }));
        }
        match Self::poll_slot(&mut self.closed, cx) {
            Poll::Ready(e) => {
                ::log::info!("MoQ client: session closed: {e}");
                Poll::Ready(e)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn stats(&self) -> impl moq_net::web_transport_trait::Stats {
        moq_net::web_transport_trait::StatsUnavailable
    }
}

// ---------------------------------------------------------------------------
// The mux pumps (browser editions of the server's `pump()`): they own both
// reads, classify each message by its first byte, and feed the queues.
// ---------------------------------------------------------------------------

/// Split a live WebTransport session into a MoQ transport and the webshooter
/// control path, and start the pumps that feed both.
///
/// `datagrams` is the writer the control path already holds: MoQ's datagrams
/// have to go out through the same one, because a `WritableStream` admits a
/// single writer and cannot be asked for a second.
///
/// The returned [`AppSide`] is where every message the mux recognised as *not*
/// MoQ lands; the caller must drain it, or the control path stops receiving.
/// Both it and the [`BrowserSession`] learn the session is over when the pumps
/// end, because a pump's exit closes every queue it fed — that is the only
/// signal a consumer parked on an empty queue can be woken by. See
/// `shared::mux` for the routing rule.
pub(crate) fn attach(
    wt: WebTransport,
    datagrams: WritableStreamDefaultWriter,
) -> (BrowserSession, AppSide) {
    let app = AppSide::default();
    let inbox = Inbox::default();
    let session = BrowserSession::over(wt.clone(), inbox.clone(), datagrams);

    let pump_wt = wt.clone();
    let pump_inbox = inbox.clone();
    let pump_app = app.clone();
    spawn_local(async move {
        if let Err(err) = drain_unistreams(pump_wt, pump_inbox.clone(), pump_app.clone()).await {
            log::debug!("unistream pump exited: {err:?}");
        }
        // One pump's end is the session's end: both readers only stop together,
        // and closing is one-way, so whichever returns first closes for good.
        // A consumer already parked on an empty queue is woken by the close.
        pump_inbox.close();
        pump_app.close();
    });

    let pump_wt = wt;
    let pump_inbox = inbox;
    let pump_app = app.clone();
    spawn_local(async move {
        if let Err(err) = drain_datagrams(pump_wt, pump_inbox.clone(), pump_app.clone()).await {
            log::debug!("datagram pump exited: {err:?}");
        }
        pump_inbox.close();
        pump_app.close();
    });

    (session, app)
}

/// Own the incoming-unistreams readable and classify what emerges: each new
/// stream costs its very first bytes a read, which names the carrier; a MoQ-
/// classified stream gets its first chunk retained for the lite driver, an app-
/// classified one is drained to the end right here (it is one full message).
async fn drain_unistreams(wt: WebTransport, inbox: Inbox, app: AppSide) -> Result<(), JsValue> {
    let incoming: ReadableStream = wt.incoming_unidirectional_streams();
    let reader: ReadableStreamDefaultReader = incoming.get_reader().dyn_into()?;
    loop {
        let read = JsFuture::from(reader.read()).await?;
        if chunk_done(&read) {
            return Ok(());
        }
        let value = match property(&read, "value") {
            Some(value) => value,
            None => continue,
        };
        let stream: ReadableStream = match value.dyn_into() {
            Ok(stream) => stream,
            Err(_) => {
                log::warn!("incoming_unidirectional_streams yielded a non-stream entry");
                continue;
            }
        };
        let inbox = inbox.clone();
        let app = app.clone();
        spawn_local(async move {
            let reader: ReadableStreamDefaultReader = match stream.get_reader().dyn_into() {
                Ok(reader) => reader,
                Err(_) => {
                    log::warn!("incoming stream has no reader");
                    return;
                }
            };
            let first_read = match JsFuture::from(reader.read()).await {
                Ok(read) => read,
                Err(err) => {
                    log::debug!("stream read failed: {err:?}");
                    return;
                }
            };
            if chunk_done(&first_read) {
                return;
            }
            let Some(value) = property(&first_read, "value") else {
                return;
            };
            let chunk = js_sys::Uint8Array::new(&value);
            let mut first = vec![0u8; chunk.length() as usize];
            chunk.copy_to(&mut first[..]);
            if first.is_empty() {
                return;
            }
            let first_byte = first[0];
            if is_moq_byte(first_byte) {
                let stream = MoQRecv::with_initial_chunk(reader, first);
                inbox.unistreams.push(stream);
                return;
            }
            // App-side stream: drain the whole message; its end is the
            // message boundary. The server already guarantees a timeout on its
            // side of the parser, so taking our time draining is fine.
            let mut payload = first;
            loop {
                let read = match JsFuture::from(reader.read()).await {
                    Ok(read) => read,
                    Err(err) => {
                        log::debug!("app stream read failed: {err:?}");
                        return;
                    }
                };
                if chunk_done(&read) {
                    break;
                }
                let Some(value) = property(&read, "value") else {
                    break;
                };
                let chunk = js_sys::Uint8Array::new(&value);
                let mut extra = vec![0u8; chunk.length() as usize];
                chunk.copy_to(&mut extra[..]);
                payload.extend_from_slice(&extra);
            }
            app.unistreams.push(payload);
        });
    }
}

/// Own the datagrams readable and route each datagram by its first byte.
async fn drain_datagrams(wt: WebTransport, inbox: Inbox, app: AppSide) -> Result<(), JsValue> {
    let datagrams: WebTransportDatagramDuplexStream = wt.datagrams();
    let reader: ReadableStreamDefaultReader = datagrams.readable().get_reader().dyn_into()?;
    loop {
        let read = JsFuture::from(reader.read()).await?;
        if chunk_done(&read) {
            return Ok(());
        }
        let Some(value) = property(&read, "value") else {
            continue;
        };
        let bytes = js_sys::Uint8Array::new(&value);
        let mut buf = vec![0u8; bytes.length() as usize];
        bytes.copy_to(&mut buf[..]);
        let payload = Bytes::from(buf);
        match payload.first() {
            Some(&byte) if is_moq_byte(byte) => inbox.datagrams.push(payload),
            // An empty datagram has no first byte to route on, so there is
            // nothing for MoQ to own: it goes to the parser, which rejects it.
            _ => app.datagrams.push(payload),
        }
    }
}

// ---------------------------------------------------------------------------
// JsValue convenience: a `{done, value}` chunk object's two fields.
// ---------------------------------------------------------------------------

fn chunk_done(chunk: &JsValue) -> bool {
    js_sys::Reflect::get(chunk, &JsValue::from_str("done"))
        .ok()
        .and_then(|d| d.as_bool())
        .unwrap_or(false)
}

fn property(value: &JsValue, key: &str) -> Option<JsValue> {
    js_sys::Reflect::get(value, &JsValue::from_str(key))
        .ok()
        .filter(|v| !v.is_undefined() && !v.is_null())
}
