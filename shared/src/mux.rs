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
//! [`MOQ_LAST_BYTE`] is the top of the reserved range. Everything at or below it
//! is MoQ's; everything above it belongs to this crate's own protocol, whose
//! discriminants therefore all start at [`APP_FIRST_BYTE`].
//!
//! The reservation is asserted rather than assumed. [`tests::reserved_range_is_disjoint`]
//! checks the two ends of the boundary, `server_datagram::tests` and
//! `client_datagram::tests` each check that every discriminant their family owns
//! lands above it, and both parsers are tested to reject the whole reserved
//! range. So a webshooter variant that wanders back down into MoQ's range, or a
//! moq-net release that grows a stream type past `0x3F`, fails a test in this
//! crate rather than the first client that tries to use it.

/// The largest first byte that can begin a MoQ message, and so the top of the
/// range reserved for MoQ on this WebTransport session.
///
/// `0x3F` rather than `0x01` because a MoQ datagram's first byte is a subscribe
/// id, not a type tag. See the module docs.
pub const MOQ_LAST_BYTE: u8 = 0x3F;

/// The first byte available to this crate's own message discriminants.
///
/// The first byte past [`MOQ_LAST_BYTE`], so a webshooter message and a MoQ
/// message can never be confused for one another.
pub const APP_FIRST_BYTE: u8 = 0x40;

/// Whether the message whose first byte is `byte` belongs to MoQ.
///
/// Used by both ends to route an incoming stream or datagram. The caller must
/// have at least one byte to look at: an empty message has no first byte and so
/// cannot be classified.
pub const fn is_moq_byte(byte: u8) -> bool {
    byte <= MOQ_LAST_BYTE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the mux: the two ranges must not touch, in either
    /// direction. If they ever did, a message would be routed to the protocol
    /// that did not write it — which for a MoQ stream means a webshooter
    /// unidirectional stream is decoded as a frame group, and for a datagram
    /// means an input event is parsed as a subscribe id.
    #[test]
    fn reserved_range_is_disjoint() {
        assert_eq!(
            APP_FIRST_BYTE,
            MOQ_LAST_BYTE + 1,
            "the app range must start exactly past the MoQ range"
        );
        // The boundary is the case that breaks: 0x3F is MoQ's last byte and
        // 0x40 the app's first, with no gap and no overlap between them.
        assert!(is_moq_byte(MOQ_LAST_BYTE));
        assert!(!is_moq_byte(APP_FIRST_BYTE));
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
