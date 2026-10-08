//! MoQ sharing the WebTransport session with webshooter's own control protocol.
//!
//! # Routing
//!
//! Nothing in the QUIC layer separates the two, so the **first byte** of every
//! incoming stream and datagram decides who owns the rest of it. The rule and the
//! reasoning behind its boundaries are in [`shared::mux`]; the short version is
//! that `0x00..=0x3F` is MoQ's and `0x40..` is webshooter's.
//!
//! # Who reads
//!
//! A QUIC connection has one reader per direction per stream type, and both
//! protocols want one. Left alone, whichever polled first would steal from the
//! other.
//!
//! So neither gets the connection directly. [`attach`] starts a single **mux
//! pump** that owns both reads, classifies what it reads, and hands each message
//! to one of four queues: MoQ's two and webshooter's two. MoQ drains its pair
//! through [`transport::WtSession`], which is where the poll traits are
//! satisfied; webshooter drains its pair through [`AppSide`].

pub(crate) mod publish;
pub(crate) mod transport;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use shared::mux::is_moq_byte;
use tokio::task::JoinHandle;
use wtransport::Connection;

use shared::wake_queue::WakeQueue;
use transport::{Error, WtRecvStream, WtSession};

/// How long to wait for a newly accepted stream to produce its first byte.
///
/// Classification has to read one byte before the stream can be routed, and that
/// read happens with the pump's other reads behind it. In practice the byte is
/// one round trip away — every protocol that opens a stream here writes its type
/// byte first, before anything else — so this is slack, not a budget. It exists
/// because without it a peer could open a stream, write nothing, and stall the
/// pump: webshooter's datagrams and every later stream would queue up behind a
/// stream that will never say what it is.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(10);

/// The MoQ half of the mux.
///
/// Fed by the pump, drained by `moq-net` through [`WtSession`].
#[derive(Clone, Default)]
pub(crate) struct Inbox {
    unistreams: WakeQueue<WtRecvStream>,
    datagrams: WakeQueue<Bytes>,
}

/// The webshooter half of the mux.
///
/// Handed to the session's control pumps, which replace their direct reads from
/// the connection with [`recv_datagram`](Self::recv_datagram) and
/// [`recv_unistream`](Self::recv_unistream). Both return `None` once the mux pump
/// has stopped, which is the only honest signal that the connection is finished: a
/// slow connection produces `Pending`, never `None`.
#[derive(Clone, Default)]
pub(crate) struct AppSide {
    unistreams: WakeQueue<WtRecvStream>,
    datagrams: WakeQueue<Bytes>,
}

impl AppSide {
    /// The next webshooter datagram, or `None` once the connection is done.
    pub(crate) async fn recv_datagram(&self) -> Option<Bytes> {
        std::future::poll_fn(|cx| self.datagrams.poll_pop(cx)).await
    }

    /// The next webshooter unidirectional stream, already classified as ours.
    ///
    /// The stream still carries its own leading byte: the pump read it to classify
    /// the stream and put it back, so callers parse the message as if nothing had
    /// touched it.
    pub(crate) async fn recv_unistream(&self) -> Option<WtRecvStream> {
        std::future::poll_fn(|cx| self.unistreams.poll_pop(cx)).await
    }
}

/// Split a live WebTransport session into a MoQ transport and the webshooter
/// control path, and start the pump that feeds both.
///
/// Returns the MoQ session, the webshooter side of the mux, and the pump task.
/// The caller must keep draining `AppSide`: the pump cannot read past a message
/// nobody takes. The caller also owns the task handle for session supervision.
pub(crate) fn attach(connection: Arc<Connection>) -> (WtSession, AppSide, JoinHandle<()>) {
    let inbox = Inbox::default();
    let app = AppSide::default();
    let session = WtSession::attach(connection.clone(), inbox.clone());
    // The pump is fed the *same* `AppSide` the caller gets below. Those two must
    // be one object: a second instance would leave the caller's queue permanently
    // empty while the client's datagrams piled up in a queue nobody drains, which
    // looks exactly like a client that has gone silent.
    let pump_task = tokio::spawn(pump(connection, inbox, app.clone()));
    (session, app, pump_task)
}

/// Own every read on the connection and route each message by its first byte.
///
/// Runs until the connection fails. On the way out it closes all four queues, so
/// a consumer parked on any of them is woken and told the session is over instead
/// of waiting for a message that can no longer arrive.
async fn pump(connection: Arc<Connection>, inbox: Inbox, app: AppSide) {
    loop {
        tokio::select! {
            biased;

            accepted = connection.accept_uni() => {
                let stream = match accepted {
                    Ok(stream) => stream,
                    Err(err) => {
                        log::debug!("connection no longer accepts unidirectional streams: {err}");
                        break;
                    }
                };

                match tokio::time::timeout(FIRST_BYTE_TIMEOUT, classify(stream)).await {
                    Ok(Ok((byte, stream))) => {
                        if is_moq_byte(byte) {
                            inbox.unistreams.push(stream);
                        } else {
                            app.unistreams.push(stream);
                        }
                    }
                    Ok(Err(err)) => {
                        // The stream is dropped here, which resets it: a peer that
                        // cannot say what a stream is has nothing to wait for.
                        log::debug!("dropping an unreadable incoming stream: {err}");
                    }
                    Err(_elapsed) => {
                        log::debug!("dropping a stream that never sent its type byte");
                    }
                }
            }

            datagram = connection.receive_datagram() => {
                match datagram {
                    Ok(datagram) => {
                        // `payload` is already stripped of the WebTransport
                        // session-id prefix, so its first byte really is the
                        // message's first byte.
                        let payload = datagram.payload();
                        match payload.first() {
                            Some(&byte) if is_moq_byte(byte) => inbox.datagrams.push(payload),
                            // An empty datagram has no first byte to route on, so
                            // there is nothing to be MoQ's. It goes to the parser,
                            // which rejects it.
                            _ => app.datagrams.push(payload),
                        }
                    }
                    Err(err) => {
                        log::debug!("connection no longer receives datagrams: {err}");
                        break;
                    }
                }
            }
        }
    }

    // Wake every consumer on every queue, or the MoQ driver and the control pumps
    // would each sit on `Pending` until the session supervisor noticed this task
    // had exited.
    inbox.unistreams.close();
    inbox.datagrams.close();
    app.unistreams.close();
    app.datagrams.close();
}

/// Read a stream's first byte and hand the stream back with it re-attached.
///
/// The byte is not consumed: the receiver has to see it, because for MoQ it *is*
/// part of the message — the `DataType` varint that says what the stream carries
/// — and for webshooter it is the message's discriminant.
async fn classify(mut stream: wtransport::stream::RecvStream) -> Result<(u8, WtRecvStream), Error> {
    let mut first = None;
    std::future::poll_fn(|cx| {
        if first.is_some() {
            return std::task::Poll::Ready(Ok(()));
        }
        let mut byte = [0u8; 1];
        match stream.quic_stream_mut().poll_read(cx, &mut byte) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            // A stream that ends before its first byte names nothing, so there is
            // no protocol to route it to.
            std::task::Poll::Ready(Ok(0)) => {
                std::task::Poll::Ready(Err(Error::closed("stream ended before its type byte")))
            }
            std::task::Poll::Ready(Ok(_)) => {
                first = Some(byte[0]);
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Ready(Err(err)) => std::task::Poll::Ready(Err(Error::Stream {
                code: 0x10e,
                reason: err.to_string(),
            })),
        }
    })
    .await?;

    let byte = first.expect("set by the poll that returned Ready");
    Ok((byte, WtRecvStream::with_first(stream, byte)))
}
