//! Real cryptography behind the `lumen-proto` seam.
//!
//! `lumen-proto` defines *what* is authenticated — which bytes, in what order,
//! under what nonce — and deliberately contains no algorithms, so that it stays
//! dependency-free for the third-party controllers that link it. This crate is
//! the other half: ChaCha20-Poly1305 and Ed25519, and nothing else.
//!
//! Keeping them apart is what lets a device swap in a vendor's hardware
//! accelerator, or a host swap in a different audited implementation, without
//! either of them touching the codec. It also means the part that is easy to get
//! subtly wrong and impossible to notice — the exact byte stream the tag covers
//! — has one definition, in the spec, tested here.
//!
//! # Portability
//!
//! `no_std`, no allocator, pure Rust. That is a requirement rather than a
//! preference: the same code has to build for riscv32imc (ESP32-C3/C6), Xtensa
//! (S3), Android and a desktop. Anything with a C dependency or an allocator
//! would rule one of those out.

#![no_std]
#![forbid(unsafe_code)]

// Tests may allocate; the crate itself may not.
#[cfg(test)]
extern crate std;

mod mesh;
mod sign;

pub use mesh::{MeshKey, KEY_LEN};
pub use sign::{Ed25519Signer, Ed25519Verifier, SecretKeyBytes};

/// Every operation in this crate fails the same way, and callers must not be
/// able to tell the failures apart.
///
/// A tag that did not verify, a key that is not a point on the curve and a
/// signature that is malformed are one condition as far as anything above is
/// concerned: the bytes are not authentic. Distinguishing them invites a caller
/// to treat one of them as recoverable, and hands an attacker an oracle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotAuthentic;

impl core::fmt::Display for NotAuthentic {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("not authentic")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;

    #[test]
    fn the_failure_reads_the_same_whatever_caused_it() {
        // The message must not hint at which check failed. A tag mismatch, a
        // key that is not a curve point and a malformed signature are one
        // condition to everything above, and a message that distinguished them
        // would be an oracle in a log line.
        assert_eq!(NotAuthentic.to_string(), "not authentic");
    }

    #[test]
    fn the_failure_is_a_plain_value() {
        // Copy and Eq so a caller can compare or store one without ceremony,
        // and no payload, so there is nothing to leak into it later.
        let e = NotAuthentic;
        assert_eq!(e, NotAuthentic);
        assert_eq!(core::mem::size_of::<NotAuthentic>(), 0);
    }
}
