//! Wire framing and codec — skeleton (W2 fills this in).
//!
//! Hand-written on purpose. The IDL in `lumen-spec` is normative, but the codec
//! here stays readable Rust; CI asserts it round-trips every codec vector in the
//! spec repo, which catches drift — the actual failure mode — with no generator
//! to maintain.

#![no_std]
#![forbid(unsafe_code)]

/// Protocol version carried in every envelope. Negotiated, so implementations
/// that fall out of step degrade visibly rather than failing mysteriously.
pub const PROTOCOL_VERSION: u8 = 0;

/// Anything that can go wrong turning bytes into messages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    /// Buffer ended mid-message.
    Truncated,
    /// A version we do not speak.
    UnsupportedVersion(u8),
    /// AEAD tag did not verify.
    BadTag,
}
