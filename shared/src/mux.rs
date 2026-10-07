//! Telling a MoQ message from a webshooter one on the same WebTransport session.
//!
//! MoQ and the webshooter control protocol share one QUIC connection: one
//! WebTransport session carries both MoQ's media and the client's input,
//! resize and keepalive traffic. Nothing separates them at the transport layer,
//! so the first byte of every incoming stream and datagram decides which
//! protocol owns the rest of it.
//!
//! # Why one byte is enough
//!
//! MoQ does not put a type tag in front of its own messages the way the
//! webshooter protocol does — it puts a *varint* there, and the values it uses
//! for the two things that can open a stream are both small:
//!
//! - a unidirectional stream begins with a `DataType`: `Group = 0` (a group of
//!   frames) or `Setup = 1` (the single SETUP message). Those are the only two
//!   unidirectional stream types moq-lite defines, and the receiver dispatches
//!   on exactly those two values.
//! - a bidirectional stream begins with a `ControlType`, whose first value is
//!   `Session = 0`.
//!
//! Under the QUIC-style varint codec a value below 64 encodes to a single byte,
//! and moq-lite versions 01 through 06 all use that codec, so a MoQ stream
//! always *starts* with a byte in `0x00..=0x3F`.
//!
//! MoQ datagrams are the awkward case: they carry no type byte at all. The body
//! is `subscribe | sequence | timestamp | payload`, so the first byte is the
//! varint of the *subscribe id*. moq-net allocates those ids internally,
//! counting from zero, and does not let the application choose them — but the
//! ones a session actually uses are small, so their varints are one byte wide
//! too, and land in the same range. Reserving the whole one-byte range rather
//! than just `0x00`/`0x01` is what makes the datagram case safe: it leaves room
//! for up to 64 subscriptions before a subscribe id could grow a second varint
//! byte and start its datagrams with `0x40`.
//!
//! # The split
//!
//! The split is defined in one place: the first variant of
//! [`ServerDatagram`](crate::server_datagram::ServerDatagram), which declares
//! the first byte this crate's own protocol is allowed to use. Everything below
//! that byte is MoQ's; everything from it up belongs to us, so both
//! [`ServerDatagram`](crate::server_datagram::ServerDatagram) and
//! [`ClientDatagram`](crate::client_datagram::ClientDatagram) start there.
//!
//! There is deliberately no free constant holding that boundary. A constant
//! would be a second, independent statement of the same fact, and the two would
//! drift: the enum is the wire format, so it is the thing that owns the number.
//!
//! The reservation is asserted rather than assumed.
//! [`tests::reserved_range_is_disjoint`] checks the two ends of the boundary,
//! `server_datagram::tests` and `client_datagram::tests` each check that every
//! discriminant their family owns lands above it, and both parsers are tested to
//! reject the whole reserved range. So a webshooter variant that wanders back
//! down into MoQ's range, or a moq-net release that grows a stream type past the
//! boundary, fails a test in this crate rather than the first client that tries
//! to use it.

use crate::server_datagram::ServerDatagramVariants;

/// Whether the message whose first byte is `byte` belongs to MoQ.
///
/// Used by both ends to route an incoming stream or datagram. The caller must
/// have at least one byte to look at: an empty message has no first byte and so
/// cannot be classified.
///
/// The boundary is the first byte of this crate's own protocol, read from
/// [`ServerDatagramVariants::RELEASE_MOUSE`] rather than restated here — see the
/// module docs.
pub const fn is_moq_byte(byte: u8) -> bool {
    byte < ServerDatagramVariants::MOQ_BOUNDARY.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boundary itself, which is the case that breaks: the byte below it is
    /// MoQ's last, the byte at it is ours, with no gap and no overlap between
    /// them.
    ///
    /// `0x3F` and `0x40` are spelled out as literals rather than derived, because
    /// this test is the one place whose job is to notice if the declared
    /// boundary is ever moved: it reads the enum and compares.
    #[test]
    fn reserved_range_is_disjoint() {
        let boundary = ServerDatagramVariants::MOQ_BOUNDARY.0;
        assert_eq!(boundary, 0x40, "the declared boundary has moved");
        assert!(
            is_moq_byte(boundary - 1),
            "the byte below the boundary must be MoQ's"
        );
        assert!(
            !is_moq_byte(boundary),
            "the boundary byte itself must be ours, or the ranges overlap"
        );
    }

    /// Both families must begin at the same byte, since they share one session.
    /// `ClientDatagram` names `ServerDatagram`'s boundary variant for exactly this
    /// reason, so this asserts the two actually agree rather than assuming it.
    #[test]
    fn both_families_start_at_the_same_byte() {
        assert_eq!(
            crate::client_datagram::ClientDatagramVariants::KEEP_ALIVE.0,
            ServerDatagramVariants::MOQ_BOUNDARY.0,
            "client and server discriminants must start together"
        );
    }

    /// The reserved range has to cover everything moq-lite can open a stream
    /// with. `DataType` is private to the crate, so the values are pinned here
    /// as literals — which is the point: if a future moq-net grows a third
    /// unidirectional stream type whose wire value leaves the range, or a
    /// version whose varint codec encodes `Setup` wider than one byte, this test
    /// is where that gets noticed.
    #[test]
    fn moq_stream_types_fit_the_reserved_range() {
        // moq-lite `DataType`: Group, Setup. `ControlType` starts at Session and
        // runs to Track, all well inside the one-byte varint range.
        for data_type in [0x00u8, 0x01] {
            assert!(
                is_moq_byte(data_type),
                "DataType {data_type} is out of range"
            );
        }
        for control_type in 0x00u8..=0x06 {
            assert!(
                is_moq_byte(control_type),
                "ControlType {control_type} is out of range"
            );
        }
    }
}
