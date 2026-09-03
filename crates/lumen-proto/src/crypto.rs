//! The cryptographic seam.
//!
//! **This module contains no cryptography.** It defines the shape of it: which
//! bytes are covered, in what order, with what nonce — and leaves the algorithms
//! to whoever links the crate.
//!
//! That split is deliberate and worth defending. `lumen-proto` has no
//! dependencies and runs on an ESP32-C3, in a phone, and in a desktop app;
//! picking a cipher implementation here would push that choice onto all three,
//! and the right choice differs (a device wants the vendor's hardware
//! acceleration, a host wants a well-audited pure-Rust crate). Meanwhile the
//! part that is easy to get subtly wrong — *what exactly is authenticated* — is
//! the part that must be identical everywhere, so it lives here and is testable
//! without any crypto at all.
//!
//! The protocol fields these traits operate on exist in the header from day one,
//! implemented or not. Adding them later would be a breaking change to every
//! implementation that existed by then.

use crate::error::{DecodeError, EncodeError};
use crate::header::{Header, HEADER_LEN, TAG_LEN};

/// Bytes of a ChaCha20-Poly1305 nonce.
pub const NONCE_LEN: usize = 12;

/// Bytes of an Ed25519 signature.
pub const SIGNATURE_LEN: usize = 64;

/// Bytes of an Ed25519 public key.
pub const PUBLIC_KEY_LEN: usize = 32;

/// An authenticated-encryption implementation, keyed with the mesh key.
///
/// **Authentication is never optional.** Encryption is: bit 0 of the header
/// flags selects encrypted-and-authenticated versus authenticated-only, because
/// pixel data and audio bands are not secret and skipping the cipher on them
/// saves cycles that matter on a C3. An implementation that ignores
/// `encrypt` and always encrypts is conforming but slow; one that
/// skips the tag is not conforming at all.
pub trait Aead {
    type Error;

    /// Authenticate, and if `encrypt` also encrypt `in_out` in place. Returns
    /// the tag.
    ///
    /// **`in_out` is authenticated either way.** The two modes are the same
    /// primitive with different inputs, not two constructions:
    ///
    /// | `encrypt` | associated data | plaintext |
    /// |---|---|---|
    /// | `true`  | `associated_data`            | `in_out` |
    /// | `false` | `associated_data` ‖ `in_out` | empty    |
    ///
    /// An implementation that authenticates only `associated_data` when
    /// `encrypt` is false leaves the payload forgeable while still producing a
    /// tag that looks valid, which is the failure this paragraph exists to
    /// prevent. See `wire-format.md` in `lumen-spec`, which is normative.
    fn seal(
        &self,
        nonce: &[u8; NONCE_LEN],
        associated_data: &[u8],
        in_out: &mut [u8],
        encrypt: bool,
    ) -> Result<[u8; TAG_LEN], Self::Error>;

    /// Verify `tag` and, if `encrypt`, decrypt `in_out` in place.
    ///
    /// Takes the same inputs as [`Aead::seal`] in the same arrangement, so a
    /// tag produced in one mode must not verify in the other.
    ///
    /// Must be constant-time in the tag comparison, and must not leave decrypted
    /// plaintext in `in_out` when verification fails.
    fn open(
        &self,
        nonce: &[u8; NONCE_LEN],
        associated_data: &[u8],
        in_out: &mut [u8],
        tag: &[u8; TAG_LEN],
        encrypt: bool,
    ) -> Result<(), Self::Error>;
}

/// Verifies Ed25519 signatures over programs and replicated records.
///
/// Separate from [`Aead`] because they answer different questions. The mesh key
/// says *this came from something inside the mesh*; a controller signature says
/// *this came from someone authorised to change what the mesh does*. A device
/// that conflated them would accept a program from any paired device.
pub trait Verifier {
    /// Whether `signature` over `message` verifies under `public_key`.
    ///
    /// Returns a plain `bool` rather than a `Result`: there is nothing to
    /// distinguish, and an error type invites a caller to treat "verification
    /// failed" as a recoverable condition.
    fn verify(
        &self,
        public_key: &[u8; PUBLIC_KEY_LEN],
        message: &[u8],
        signature: &[u8; SIGNATURE_LEN],
    ) -> bool;
}

/// Signs. Only a controller holds a key; a device signs its own record with its
/// identity key and nothing else.
pub trait Signer {
    type Error;

    fn sign(&self, message: &[u8]) -> Result<[u8; SIGNATURE_LEN], Self::Error>;
}

/// A sealed datagram, ready to send.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sealed {
    /// Total bytes written into the caller's buffer.
    pub len: usize,
}

/// Seal a datagram in place.
///
/// `buf` must already hold the header at the front and the plaintext payload
/// after it; the tag is written after the payload. This exists so no
/// implementation has to decide for itself what the associated data is — it is
/// the header, exactly as it will be sent, and getting that wrong is both easy
/// and invisible until someone else refuses your packets.
pub fn seal_in_place<A: Aead>(
    aead: &A,
    buf: &mut [u8],
    payload_len: usize,
    boot_counter: u32,
) -> Result<Sealed, EncodeError> {
    let total = HEADER_LEN + payload_len + TAG_LEN;
    if buf.len() < total {
        return Err(EncodeError::BufferTooSmall {
            needed: total,
            available: buf.len(),
        });
    }
    let header = Header::decode(&buf[..HEADER_LEN]).map_err(EncodeError::Invalid)?;
    let nonce = header.nonce(boot_counter);
    let encrypt = header.flags.is_encrypted();

    // The header is the associated data. Split so the payload can be mutated
    // while the header is read, without copying either.
    let (head, rest) = buf.split_at_mut(HEADER_LEN);
    let (payload, tail) = rest.split_at_mut(payload_len);

    let tag = aead
        .seal(&nonce, head, payload, encrypt)
        .map_err(|_| EncodeError::Invalid(DecodeError::BadTag))?;
    tail[..TAG_LEN].copy_from_slice(&tag);
    Ok(Sealed { len: total })
}

/// Open a received datagram in place, returning the plaintext payload.
///
/// On failure the payload is **not** returned and must be treated as absent:
/// a caller that reads it anyway is reading unauthenticated bytes, which is the
/// whole thing this prevents.
pub fn open_in_place<'a, A: Aead>(
    aead: &A,
    buf: &'a mut [u8],
    boot_counter: u32,
) -> Result<(Header, &'a [u8]), DecodeError> {
    if buf.len() < HEADER_LEN + TAG_LEN {
        return Err(DecodeError::Truncated);
    }
    let header = Header::decode(&buf[..HEADER_LEN])?;
    let payload_len = header.payload_len as usize;
    if HEADER_LEN + payload_len + TAG_LEN != buf.len() {
        return Err(DecodeError::PayloadLenMismatch {
            declared: header.payload_len,
            available: buf.len() - HEADER_LEN - TAG_LEN,
        });
    }
    let nonce = header.nonce(boot_counter);
    let encrypt = header.flags.is_encrypted();

    let (head, rest) = buf.split_at_mut(HEADER_LEN);
    let (payload, tail) = rest.split_at_mut(payload_len);
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&tail[..TAG_LEN]);

    aead.open(&nonce, head, payload, &tag, encrypt)
        .map_err(|_| DecodeError::BadTag)?;
    Ok((header, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{Flags, MsgType};
    use crate::Uuid;

    /// A stand-in cipher: XOR with a nonce byte, and a tag that is a checksum
    /// over the associated data and the ciphertext.
    ///
    /// Not cryptography and not pretending to be. Its job is to prove the
    /// *framing* is right — that the header is what gets authenticated, that the
    /// tag lands after the payload, and that a modified header is refused —
    /// which is the part this module is actually responsible for.
    struct XorAead;

    impl Aead for XorAead {
        type Error = ();

        fn seal(
            &self,
            nonce: &[u8; NONCE_LEN],
            associated_data: &[u8],
            in_out: &mut [u8],
            encrypt: bool,
        ) -> Result<[u8; TAG_LEN], ()> {
            if encrypt {
                for (i, b) in in_out.iter_mut().enumerate() {
                    *b ^= nonce[i % NONCE_LEN];
                }
            }
            Ok(checksum(associated_data, in_out))
        }

        fn open(
            &self,
            nonce: &[u8; NONCE_LEN],
            associated_data: &[u8],
            in_out: &mut [u8],
            tag: &[u8; TAG_LEN],
            encrypt: bool,
        ) -> Result<(), ()> {
            if checksum(associated_data, in_out) != *tag {
                return Err(());
            }
            if encrypt {
                for (i, b) in in_out.iter_mut().enumerate() {
                    *b ^= nonce[i % NONCE_LEN];
                }
            }
            Ok(())
        }
    }

    fn checksum(ad: &[u8], payload: &[u8]) -> [u8; TAG_LEN] {
        let mut t = [0u8; TAG_LEN];
        for (i, b) in ad.iter().chain(payload).enumerate() {
            t[i % TAG_LEN] ^= b.wrapping_add(i as u8);
        }
        t
    }

    fn datagram(encrypted: bool, payload: &[u8]) -> ([u8; 128], usize) {
        let mut buf = [0u8; 128];
        let mut h = Header::new(MsgType::Chan, [1, 2], [3, 4, 5, 6], 9, 1234);
        if encrypted {
            h.flags = Flags::empty().with(Flags::ENCRYPTED);
        }
        h.payload_len = payload.len() as u16;
        h.encode(&mut buf[..HEADER_LEN]).unwrap();
        buf[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
        (buf, payload.len())
    }

    #[test]
    fn a_sealed_datagram_opens_again() {
        for encrypted in [false, true] {
            let plain = [1u8, 2, 3, 4, 5];
            let (mut buf, len) = datagram(encrypted, &plain);
            let sealed = seal_in_place(&XorAead, &mut buf, len, 7).unwrap();
            assert_eq!(sealed.len, HEADER_LEN + len + TAG_LEN);

            let (header, payload) = open_in_place(&XorAead, &mut buf[..sealed.len], 7).unwrap();
            assert_eq!(payload, &plain);
            assert_eq!(header.flags.is_encrypted(), encrypted);
        }
    }

    #[test]
    fn encryption_actually_changes_the_bytes_and_authentication_alone_does_not() {
        // Bit 0 selects encrypted-and-authenticated versus authenticated-only.
        // Pixel data is not secret, and skipping the cipher on it saves cycles
        // that matter on a C3 - but the tag is there either way.
        let plain = [9u8; 8];

        let (mut clear, len) = datagram(false, &plain);
        seal_in_place(&XorAead, &mut clear, len, 1).unwrap();
        assert_eq!(&clear[HEADER_LEN..HEADER_LEN + len], &plain);

        let (mut secret, len2) = datagram(true, &plain);
        seal_in_place(&XorAead, &mut secret, len2, 1).unwrap();
        assert_ne!(&secret[HEADER_LEN..HEADER_LEN + len2], &plain);
    }

    #[test]
    fn tampering_with_the_header_is_caught() {
        // The reason the header is the associated data. Someone who could
        // rewrite `show_time_us` or the message type without invalidating the
        // tag could replay a packet as something else entirely.
        let plain = [1u8, 2, 3];
        let (mut buf, len) = datagram(false, &plain);
        let sealed = seal_in_place(&XorAead, &mut buf, len, 3).unwrap();

        // Type and show_time: caught by the tag, because they are authenticated
        // but not structural.
        for byte in [2usize, 14] {
            let mut tampered = buf;
            tampered[byte] ^= 0xFF;
            assert_eq!(
                open_in_place(&XorAead, &mut tampered[..sealed.len], 3).err(),
                Some(DecodeError::BadTag),
                "modifying header byte {byte} was not caught"
            );
        }
        // payload_len is caught earlier and more specifically, by the framing
        // check - which is better, not worse: it says what is wrong.
        let mut relen = buf;
        relen[22] ^= 0xFF;
        assert!(matches!(
            open_in_place(&XorAead, &mut relen[..sealed.len], 3),
            Err(DecodeError::PayloadLenMismatch { .. })
        ));
    }

    #[test]
    fn tampering_with_the_payload_is_caught() {
        let plain = [1u8, 2, 3, 4];
        let (mut buf, len) = datagram(true, &plain);
        let sealed = seal_in_place(&XorAead, &mut buf, len, 3).unwrap();
        buf[HEADER_LEN + 1] ^= 0x01;
        assert_eq!(
            open_in_place(&XorAead, &mut buf[..sealed.len], 3).err(),
            Some(DecodeError::BadTag)
        );
    }

    /// Records the nonce it was handed, and does nothing else.
    ///
    /// What this module owes the caller is that the right nonce reaches the
    /// cipher. Whether a wrong nonce then fails to decrypt is the cipher's
    /// property, not the seam's, and asserting it through a stand-in would only
    /// test the stand-in.
    struct NonceSpy(core::cell::Cell<[u8; NONCE_LEN]>);

    impl Aead for NonceSpy {
        type Error = ();
        fn seal(
            &self,
            nonce: &[u8; NONCE_LEN],
            _ad: &[u8],
            _in_out: &mut [u8],
            _encrypt: bool,
        ) -> Result<[u8; TAG_LEN], ()> {
            self.0.set(*nonce);
            Ok([0u8; TAG_LEN])
        }
        fn open(
            &self,
            nonce: &[u8; NONCE_LEN],
            _ad: &[u8],
            _in_out: &mut [u8],
            _tag: &[u8; TAG_LEN],
            _encrypt: bool,
        ) -> Result<(), ()> {
            self.0.set(*nonce);
            Ok(())
        }
    }

    #[test]
    fn the_boot_counter_reaches_the_cipher_in_the_nonce() {
        // Without it, a device that rebooted would restart its sequence at zero
        // and reuse nonces under the same key - the classic way to destroy a
        // stream cipher. The seam's job is to make sure it gets there.
        let plain = [7u8; 6];
        let spy = NonceSpy(core::cell::Cell::new([0; NONCE_LEN]));

        let (mut a, len) = datagram(true, &plain);
        seal_in_place(&spy, &mut a, len, 1).unwrap();
        let first = spy.0.get();

        let (mut b, len2) = datagram(true, &plain);
        seal_in_place(&spy, &mut b, len2, 2).unwrap();
        let second = spy.0.get();

        assert_ne!(first, second, "the boot counter did not reach the nonce");
        assert_eq!(
            &first[..8],
            &second[..8],
            "only the boot counter should differ"
        );
        assert_eq!(&first[8..], &1u32.to_le_bytes());
        assert_eq!(&second[8..], &2u32.to_le_bytes());
    }

    #[test]
    fn sealing_into_a_short_buffer_is_refused() {
        let mut small = [0u8; 8];
        assert!(matches!(
            seal_in_place(&XorAead, &mut small, 4, 0),
            Err(EncodeError::BufferTooSmall { .. })
        ));
    }

    #[test]
    fn opening_a_malformed_datagram_is_refused_without_panicking() {
        let plain = [1u8, 2, 3];
        let (mut buf, len) = datagram(false, &plain);
        let sealed = seal_in_place(&XorAead, &mut buf, len, 0).unwrap();
        for cut in 0..sealed.len {
            let mut copy = buf;
            let _ = open_in_place(&XorAead, &mut copy[..cut], 0);
        }
    }

    #[test]
    fn a_signature_preimage_is_defined_in_exactly_one_place() {
        // Documented here so it is impossible to miss: the record signature
        // covers record_id, record_type, hlc, author and body, in that order,
        // and `StateRecord::signed_bytes_into` is the only thing that writes it.
        let sig = [0u8; SIGNATURE_LEN];
        let rec = crate::msg::StateRecord {
            record_id: Uuid([1; 16]),
            record_type: 2,
            hlc: 3,
            author: Uuid([4; 16]),
            body: &[5, 6],
            sig: &sig,
        };
        let mut out = [0u8; 64];
        let n = rec.signed_bytes_into(&mut out).unwrap();
        assert_eq!(n, rec.signed_len());
    }

    #[test]
    fn the_constants_match_the_algorithms_they_name() {
        assert_eq!(NONCE_LEN, 12, "ChaCha20-Poly1305 nonce");
        assert_eq!(TAG_LEN, 16, "Poly1305 tag");
        assert_eq!(SIGNATURE_LEN, 64, "Ed25519 signature");
        assert_eq!(PUBLIC_KEY_LEN, 32, "Ed25519 public key");
    }
}
