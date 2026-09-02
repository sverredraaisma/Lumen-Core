//! Wire framing and codec for the Lumen protocol.
//!
//! Hand-written on purpose. The IDL in `lumen-spec` is normative, but the codec
//! here stays readable Rust; CI asserts it round-trips every codec vector in the
//! spec repo, which catches drift — the actual failure mode — with no generator
//! to maintain.
//!
//! # Shape
//!
//! ```text
//! Datagram  =  Header (24 B)  ‖  payload (n B)  ‖  AEAD tag (16 B)
//! ```
//!
//! [`Datagram`] handles framing and leaves the payload as bytes; [`Payload`]
//! parses those bytes according to the header's type. The two are separate
//! because a receiver decides whether a packet is worth decrypting **from the
//! header alone** — `show_time_us` says whether it is already late, and
//! `mesh_prefix` says whether it belongs to this mesh at all.
//!
//! Cryptography is not here. The AEAD tag is carried as opaque bytes and
//! verified by the layer that holds the mesh key (W14); this crate's job is to
//! say exactly which bytes are covered.
//!
//! # Rules that decoding enforces
//!
//! - **An unknown message type is ignored, not rejected.** That is what makes
//!   minor-version additions safe.
//! - **Reserved fields are ignored on receive**, never rejected.
//! - **Trailing bytes after a payload are not an error** — a peer one minor
//!   version ahead may have appended a field.
//! - **A `SRC_PUSH` above the ambient floor with no expiry is refused**, on
//!   encode as well as decode. That is the "stuck red at 3am" rule made
//!   unreachable rather than merely discouraged.

#![no_std]
#![forbid(unsafe_code)]

pub mod buf;
pub mod error;
pub mod header;
pub mod msg;
pub mod replay;

pub use buf::{Reader, Writer};
pub use error::{DecodeError, EncodeError};
pub use header::{
    Flags, Header, MsgType, HEADER_LEN, MAGIC, OVERHEAD, TAG_LEN, VERSION_MAJOR, VERSION_MINOR,
};
pub use msg::Payload;
pub use replay::{ReplayVerdict, ReplayWindow};

/// Protocol version byte as it appears on the wire: `major << 4 | minor`.
pub const PROTOCOL_VERSION: u8 = (VERSION_MAJOR << 4) | VERSION_MINOR;

/// A 16-byte identifier.
///
/// Deliberately not a general-purpose UUID type: the protocol only ever needs
/// sixteen opaque bytes, and pulling in a dependency for that would reach every
/// device in the mesh.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct Uuid(pub [u8; 16]);

impl Uuid {
    pub const NIL: Uuid = Uuid([0; 16]);

    pub const fn from_bytes(b: [u8; 16]) -> Uuid {
        Uuid(b)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// The first two bytes, as they appear in a header's `mesh_prefix`.
    pub const fn mesh_prefix(&self) -> [u8; 2] {
        [self.0[0], self.0[1]]
    }

    /// The first four bytes, as they appear in a header's `sender_prefix`.
    pub const fn sender_prefix(&self) -> [u8; 4] {
        [self.0[0], self.0[1], self.0[2], self.0[3]]
    }
}

/// A complete datagram: header, payload bytes, and the AEAD tag.
///
/// The payload is **not** parsed here and may still be ciphertext. Framing and
/// meaning are separate steps so that a receiver can drop a late or foreign
/// packet before spending a decrypt on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Datagram<'a> {
    pub header: Header,
    /// Plaintext or ciphertext according to [`Flags::is_encrypted`].
    pub payload: &'a [u8],
    pub tag: &'a [u8; TAG_LEN],
}

impl<'a> Datagram<'a> {
    /// Split a received buffer into header, payload and tag.
    pub fn decode(buf: &'a [u8]) -> Result<Datagram<'a>, DecodeError> {
        let header = Header::decode(buf)?;
        // Check the fixed cost is present before subtracting it. Saturating here
        // instead would let a header-only buffer with payload_len 0 pass the
        // length check and then panic slicing out a tag that is not there.
        if buf.len() < HEADER_LEN + TAG_LEN {
            return Err(DecodeError::Truncated);
        }
        let declared = header.payload_len as usize;
        let available = buf.len() - HEADER_LEN - TAG_LEN;
        if available != declared {
            return Err(DecodeError::PayloadLenMismatch {
                declared: header.payload_len,
                available,
            });
        }
        let payload = &buf[HEADER_LEN..HEADER_LEN + declared];
        let tag_bytes = &buf[HEADER_LEN + declared..HEADER_LEN + declared + TAG_LEN];
        let tag: &[u8; TAG_LEN] = tag_bytes.try_into().map_err(|_| DecodeError::Truncated)?;
        Ok(Datagram {
            header,
            payload,
            tag,
        })
    }

    /// The bytes the AEAD treats as associated data: the header, exactly as
    /// received.
    ///
    /// Defined here rather than at the call site so a signer and a verifier
    /// cannot disagree about the boundary.
    pub fn associated_data(buf: &[u8]) -> Result<&[u8], DecodeError> {
        if buf.len() < HEADER_LEN {
            return Err(DecodeError::Truncated);
        }
        Ok(&buf[..HEADER_LEN])
    }

    /// Total encoded size for a payload of `payload_len` bytes.
    pub const fn encoded_len(payload_len: usize) -> usize {
        HEADER_LEN + payload_len + TAG_LEN
    }

    /// Write header, payload and tag into `out`, returning the bytes written.
    ///
    /// `payload_len` in the supplied header is ignored and recomputed, so the
    /// two can never disagree on the wire.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, EncodeError> {
        let total = Self::encoded_len(self.payload.len());
        if out.len() < total {
            return Err(EncodeError::BufferTooSmall {
                needed: total,
                available: out.len(),
            });
        }
        if self.payload.len() > u16::MAX as usize {
            return Err(EncodeError::BlobTooLong {
                len: self.payload.len(),
            });
        }
        let mut header = self.header;
        header.payload_len = self.payload.len() as u16;
        header.encode(&mut out[..HEADER_LEN])?;
        out[HEADER_LEN..HEADER_LEN + self.payload.len()].copy_from_slice(self.payload);
        out[HEADER_LEN + self.payload.len()..total].copy_from_slice(self.tag);
        Ok(total)
    }

    /// Parse the payload according to the header's type.
    ///
    /// `Ok(None)` means the type is one this implementation does not know, which
    /// is **not** an error — the datagram is simply ignored.
    pub fn parse_payload(&self) -> Result<Option<Payload<'a>>, DecodeError> {
        match self.header.typed() {
            Some(t) => Payload::decode(t, self.payload).map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG: [u8; TAG_LEN] = [0xAA; TAG_LEN];

    fn header_for(payload_len: u16) -> Header {
        let mut h = Header::new(MsgType::SyncReq, [1, 2], [3, 4, 5, 6], 42, 999);
        h.payload_len = payload_len;
        h
    }

    #[test]
    fn protocol_version_packs_major_and_minor() {
        assert_eq!(PROTOCOL_VERSION, 0x01);
    }

    #[test]
    fn uuid_prefixes_come_off_the_front() {
        let id = Uuid::from_bytes([9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(id.mesh_prefix(), [9, 8]);
        assert_eq!(id.sender_prefix(), [9, 8, 7, 6]);
        assert_eq!(id.as_bytes()[15], 6);
        assert_eq!(Uuid::NIL, Uuid::default());
    }

    #[test]
    fn datagram_round_trips() {
        let payload = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let dg = Datagram {
            header: header_for(payload.len() as u16),
            payload: &payload,
            tag: &TAG,
        };

        let mut buf = [0u8; 64];
        let n = dg.encode(&mut buf).unwrap();
        assert_eq!(n, Datagram::encoded_len(payload.len()));
        assert_eq!(n, 24 + 8 + 16);

        let back = Datagram::decode(&buf[..n]).unwrap();
        assert_eq!(back, dg);
    }

    #[test]
    fn encode_recomputes_payload_len_rather_than_trusting_it() {
        // A header claiming the wrong length must not be able to produce a
        // datagram that decodes to something else.
        let payload = [1u8, 2, 3];
        let dg = Datagram {
            header: header_for(9999),
            payload: &payload,
            tag: &TAG,
        };
        let mut buf = [0u8; 64];
        let n = dg.encode(&mut buf).unwrap();
        let back = Datagram::decode(&buf[..n]).unwrap();
        assert_eq!(back.header.payload_len, 3);
    }

    #[test]
    fn a_declared_length_that_does_not_match_the_bytes_is_rejected() {
        let payload = [1u8, 2, 3];
        let dg = Datagram {
            header: header_for(3),
            payload: &payload,
            tag: &TAG,
        };
        let mut buf = [0u8; 64];
        let n = dg.encode(&mut buf).unwrap();

        // Claim one byte more than is present.
        buf[22] = 4;
        assert_eq!(
            Datagram::decode(&buf[..n]),
            Err(DecodeError::PayloadLenMismatch {
                declared: 4,
                available: 3
            })
        );
    }

    #[test]
    fn a_datagram_shorter_than_header_plus_tag_is_rejected() {
        let payload: [u8; 0] = [];
        let dg = Datagram {
            header: header_for(0),
            payload: &payload,
            tag: &TAG,
        };
        let mut buf = [0u8; 64];
        let n = dg.encode(&mut buf).unwrap();
        assert_eq!(n, OVERHEAD);

        // Every truncation of a minimal datagram must fail, never panic.
        for len in 0..n {
            assert!(
                Datagram::decode(&buf[..len]).is_err(),
                "length {len} should not decode"
            );
        }
    }

    #[test]
    fn encoding_into_a_short_buffer_reports_what_was_needed() {
        let payload = [0u8; 10];
        let dg = Datagram {
            header: header_for(10),
            payload: &payload,
            tag: &TAG,
        };
        let mut small = [0u8; 8];
        assert_eq!(
            dg.encode(&mut small),
            Err(EncodeError::BufferTooSmall {
                needed: 50,
                available: 8
            })
        );
    }

    #[test]
    fn associated_data_is_exactly_the_header() {
        let payload = [7u8; 4];
        let dg = Datagram {
            header: header_for(4),
            payload: &payload,
            tag: &TAG,
        };
        let mut buf = [0u8; 64];
        let n = dg.encode(&mut buf).unwrap();

        let ad = Datagram::associated_data(&buf[..n]).unwrap();
        assert_eq!(ad.len(), HEADER_LEN);
        assert_eq!(ad, &buf[..HEADER_LEN]);

        assert_eq!(
            Datagram::associated_data(&buf[..HEADER_LEN - 1]),
            Err(DecodeError::Truncated)
        );
    }

    #[test]
    fn an_unknown_type_parses_to_none_rather_than_an_error() {
        let payload = [0u8; 4];
        let mut header = header_for(4);
        header.msg_type = 0xF5; // vendor range: never assigned by the spec
        let dg = Datagram {
            header,
            payload: &payload,
            tag: &TAG,
        };
        let mut buf = [0u8; 64];
        let n = dg.encode(&mut buf).unwrap();

        let back = Datagram::decode(&buf[..n]).unwrap();
        assert_eq!(back.parse_payload(), Ok(None));
    }

    #[test]
    fn a_known_type_parses_to_its_payload() {
        let mut body = [0u8; 8];
        Writer::new(&mut body).u64(0x1122).unwrap();
        let dg = Datagram {
            header: header_for(8),
            payload: &body,
            tag: &TAG,
        };
        assert_eq!(
            dg.parse_payload().unwrap(),
            Some(Payload::SyncReq(msg::SyncReq { t1: 0x1122 }))
        );
    }
}
