//! Decode and encode failures.
//!
//! Two rules from the wire format shape this enum, and neither is negotiable:
//!
//! - **An unknown message type is ignored, not an error.** That is what makes
//!   minor-version additions safe, so [`DecodeError`] has no "unknown type"
//!   variant at all — [`crate::MsgType::from_u8`] returns an `Option` and the
//!   caller drops the datagram.
//! - **Reserved fields are ignored on receive, never rejected.** So there is no
//!   variant for a non-zero reserved byte either.

/// Why a byte slice could not be turned into a message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    /// Ran off the end of the buffer.
    Truncated,
    /// First byte was not [`crate::MAGIC`]. Almost always another protocol on a
    /// shared port rather than corruption.
    BadMagic(u8),
    /// Major version we do not implement. Minor versions are forward-compatible
    /// by construction and never land here.
    UnsupportedVersion { major: u8, minor: u8 },
    /// `payload_len` in the header disagrees with the bytes actually present.
    PayloadLenMismatch { declared: u16, available: usize },
    /// A `str` field was not valid UTF-8.
    BadUtf8,
    /// A field held a value outside its defined range.
    InvalidValue { field: &'static str },
    /// The AEAD tag did not verify.
    ///
    /// Deliberately carries nothing about *why*. There is one useful response -
    /// drop the datagram - and any detail here would be a signal to whoever sent
    /// it about how close they got.
    BadTag,
    /// A `SRC_PUSH` above the ambient floor carried no expiry.
    ///
    /// The "stuck red at 3am" rule, enforced at the wire level so that no client
    /// can create the condition even by accident. See
    /// [`crate::msg::SrcPush`].
    SourceWithoutExpiry { priority: u8 },
}

/// Why a message could not be written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EncodeError {
    /// The destination buffer was too small.
    BufferTooSmall { needed: usize, available: usize },
    /// A `str` field exceeded the 255-byte wire limit.
    StringTooLong { len: usize },
    /// A `blob` field exceeded the 65535-byte wire limit.
    BlobTooLong { len: usize },
    /// The value violates a protocol invariant and must never reach the wire.
    ///
    /// Encoding rejects the same things decoding does, so a bug cannot produce a
    /// datagram that a conforming receiver would refuse.
    Invalid(DecodeError),
}

impl From<DecodeError> for EncodeError {
    fn from(e: DecodeError) -> Self {
        EncodeError::Invalid(e)
    }
}
