//! The L1 header, on every datagram, on every transport.
//!
//! ```text
//! off  size  field
//! 0    1     magic          0x4C
//! 1    1     version        major<<4 | minor
//! 2    1     type
//! 3    1     flags          bit0 encrypted, bit1 fragment, bit2 last-fragment
//! 4    2     mesh_prefix    first 2 bytes of mesh_id
//! 6    4     sender_prefix  first 4 bytes of sender uuid
//! 10   4     sequence       per sender, per boot
//! 14   8     show_time_us   when this payload is VALID, not when it was sent
//! 22   2     payload_len
//! 24   n     payload
//! 24+n 16    AEAD tag
//! ```
//!
//! Two fields are placed where they are for a reason worth preserving.
//! `show_time_us` sits in the header so a receiver can discard a late packet
//! without parsing or decrypting it, and `mesh_prefix` lets a device on a shared
//! LAN drop another mesh's traffic on a two-byte comparison — the AEAD tag would
//! reject it anyway, but only after a wasted decrypt, and at 50 devices × 60 Hz
//! that waste is real.

use crate::buf::{Reader, Writer};
use crate::error::{DecodeError, EncodeError};

/// First byte of every datagram. ASCII `L`.
pub const MAGIC: u8 = 0x4C;

/// Major version this implementation speaks. A different major is refused.
pub const VERSION_MAJOR: u8 = 0;

/// Minor version this implementation speaks. A *higher* minor from a peer is
/// accepted: unknown message types within a major version are ignored, which is
/// what makes minor additions safe.
pub const VERSION_MINOR: u8 = 1;

/// Encoded size of the header.
pub const HEADER_LEN: usize = 24;

/// Size of the trailing AEAD tag (Poly1305).
pub const TAG_LEN: usize = 16;

/// Header plus tag — the fixed cost of a datagram.
pub const OVERHEAD: usize = HEADER_LEN + TAG_LEN;

/// The largest datagram this protocol sends, header and tag included.
///
/// 1200 rather than something near a 1500-byte Ethernet MTU because a
/// surprising number of home networks have a tunnel somewhere in the path — a
/// VPN, a mesh-WiFi backhaul, a carrier doing PPPoE — and each shaves the usable
/// size. Fragmentation exists for what genuinely needs more, but a show that
/// fragments every frame has turned one lost packet into two, so the common path
/// is sized to fit.
///
/// The limit is on the whole datagram rather than the payload inside it: what
/// has to survive the path is the packet, and a rule about the payload alone
/// would be [`OVERHEAD`] bytes wrong in exactly the case where it matters.
pub const MAX_DATAGRAM: usize = 1200;

/// The largest message payload that fits in a datagram without fragmenting.
///
/// What the compiler assumes when sizing a `CHAN` payload.
pub const MAX_PAYLOAD: usize = MAX_DATAGRAM - OVERHEAD;

/// Header `flags` bits. Everything not named here is reserved: zero on send,
/// ignored on receive.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Flags(pub u8);

impl Flags {
    /// Payload is encrypted as well as authenticated.
    ///
    /// Clear means authenticated-only: pixel data and audio bands are not
    /// secret, and skipping the cipher on them saves cycles that matter on a C3.
    /// **Authentication is never optional**, in either case.
    pub const ENCRYPTED: u8 = 1 << 0;
    /// This datagram is one fragment of a larger message.
    pub const FRAGMENT: u8 = 1 << 1;
    /// This is the final fragment.
    pub const LAST_FRAGMENT: u8 = 1 << 2;

    pub const fn empty() -> Self {
        Flags(0)
    }

    pub const fn contains(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    #[must_use]
    pub const fn with(self, bit: u8) -> Self {
        Flags(self.0 | bit)
    }

    pub const fn is_encrypted(self) -> bool {
        self.contains(Self::ENCRYPTED)
    }

    pub const fn is_fragment(self) -> bool {
        self.contains(Self::FRAGMENT)
    }

    pub const fn is_last_fragment(self) -> bool {
        self.contains(Self::LAST_FRAGMENT)
    }
}

/// Message type. The high nibble is the category, which keeps dispatch a jump
/// table and leaves obvious room to grow.
///
/// An unknown code is **ignored, not an error** — [`MsgType::from_u8`] returns
/// `None` and the caller drops the datagram. That rule is what makes
/// minor-version additions safe, so do not turn it into a `DecodeError`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum MsgType {
    Hello = 0x01,
    Caps = 0x02,
    Get = 0x03,
    Set = 0x04,

    Tick = 0x10,
    SyncReq = 0x11,
    SyncResp = 0x12,

    Activate = 0x20,
    Chan = 0x21,
    ChanClaim = 0x22,
    ChanRelease = 0x23,
    Frame = 0x24,

    SrcPush = 0x30,
    SrcRenew = 0x31,
    SrcPop = 0x32,

    Event = 0x40,

    StateDigest = 0x50,
    StatePull = 0x51,
    StatePush = 0x52,

    ProgBegin = 0x60,
    ProgChunk = 0x61,
    ProgEnd = 0x62,

    FedHello = 0x70,
    FedEvent = 0x71,
    FedCue = 0x72,

    ProbeSet = 0x80,
    ProbeData = 0x81,
    TimeCtl = 0x82,
}

impl MsgType {
    /// Map a wire code to a type, or `None` if this implementation does not know
    /// it. `None` means ignore the datagram, never reject the sender.
    pub const fn from_u8(v: u8) -> Option<MsgType> {
        use MsgType::*;
        Some(match v {
            0x01 => Hello,
            0x02 => Caps,
            0x03 => Get,
            0x04 => Set,
            0x10 => Tick,
            0x11 => SyncReq,
            0x12 => SyncResp,
            0x20 => Activate,
            0x21 => Chan,
            0x22 => ChanClaim,
            0x23 => ChanRelease,
            0x24 => Frame,
            0x30 => SrcPush,
            0x31 => SrcRenew,
            0x32 => SrcPop,
            0x40 => Event,
            0x50 => StateDigest,
            0x51 => StatePull,
            0x52 => StatePush,
            0x60 => ProgBegin,
            0x61 => ProgChunk,
            0x62 => ProgEnd,
            0x70 => FedHello,
            0x71 => FedEvent,
            0x72 => FedCue,
            0x80 => ProbeSet,
            0x81 => ProbeData,
            0x82 => TimeCtl,
            _ => return None,
        })
    }

    /// The wire code.
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// True for codes the spec reserves for vendor and experimental use and will
    /// never assign. A conforming implementation ignores them like any other
    /// unknown type; this predicate exists so a gateway can tell "not mine" from
    /// "not yet invented" when logging.
    pub const fn is_vendor_code(v: u8) -> bool {
        v >= 0xF0
    }
}

/// The decoded L1 header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub version_major: u8,
    pub version_minor: u8,
    pub msg_type: u8,
    pub flags: Flags,
    pub mesh_prefix: [u8; 2],
    pub sender_prefix: [u8; 4],
    pub sequence: u32,
    /// When this payload is **valid**, not when it was sent.
    pub show_time_us: u64,
    pub payload_len: u16,
}

impl Header {
    /// A header for a locally originated datagram, at the version this
    /// implementation speaks.
    pub fn new(
        msg_type: MsgType,
        mesh_prefix: [u8; 2],
        sender_prefix: [u8; 4],
        sequence: u32,
        show_time_us: u64,
    ) -> Self {
        Header {
            version_major: VERSION_MAJOR,
            version_minor: VERSION_MINOR,
            msg_type: msg_type.to_u8(),
            flags: Flags::empty(),
            mesh_prefix,
            sender_prefix,
            sequence,
            show_time_us,
            payload_len: 0,
        }
    }

    /// The known message type, or `None` for one this implementation ignores.
    pub const fn typed(&self) -> Option<MsgType> {
        MsgType::from_u8(self.msg_type)
    }

    /// The 12-byte AEAD nonce: `sender_prefix ‖ sequence ‖ boot_counter`.
    ///
    /// `boot_counter` comes from NVS and increments on every boot. Without it a
    /// device rebooting would restart its sequence at zero and reuse nonces under
    /// the same key, which is the classic way to destroy a stream cipher. With
    /// it, reuse needs 2³² datagrams inside one boot — over a year at 100/s.
    pub fn nonce(&self, boot_counter: u32) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[0..4].copy_from_slice(&self.sender_prefix);
        n[4..8].copy_from_slice(&self.sequence.to_le_bytes());
        n[8..12].copy_from_slice(&boot_counter.to_le_bytes());
        n
    }

    /// Decode a header from the front of `buf`.
    ///
    /// Does not check that the payload is actually present; see
    /// [`crate::Datagram::decode`] for the framing-level check.
    pub fn decode(buf: &[u8]) -> Result<Header, DecodeError> {
        let mut r = Reader::new(buf);
        let magic = r.u8()?;
        if magic != MAGIC {
            return Err(DecodeError::BadMagic(magic));
        }
        let version = r.u8()?;
        let version_major = version >> 4;
        let version_minor = version & 0x0F;
        if version_major != VERSION_MAJOR {
            return Err(DecodeError::UnsupportedVersion {
                major: version_major,
                minor: version_minor,
            });
        }
        let msg_type = r.u8()?;
        let flags = Flags(r.u8()?);
        let mesh_prefix = r.array::<2>()?;
        let sender_prefix = r.array::<4>()?;
        let sequence = r.u32()?;
        let show_time_us = r.u64()?;
        let payload_len = r.u16()?;

        Ok(Header {
            version_major,
            version_minor,
            msg_type,
            flags,
            mesh_prefix,
            sender_prefix,
            sequence,
            show_time_us,
            payload_len,
        })
    }

    /// Encode into the front of `buf`, returning the bytes written.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
        let mut w = Writer::new(buf);
        w.u8(MAGIC)?;
        w.u8((self.version_major << 4) | (self.version_minor & 0x0F))?;
        w.u8(self.msg_type)?;
        w.u8(self.flags.0)?;
        w.bytes(&self.mesh_prefix)?;
        w.bytes(&self.sender_prefix)?;
        w.u32(self.sequence)?;
        w.u64(self.show_time_us)?;
        w.u16(self.payload_len)?;
        Ok(w.position())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_size_limits_agree_with_each_other() {
        // Stated separately because callers want each, and derived so they
        // cannot drift: a payload limit that did not account for the header and
        // tag would overflow the datagram by exactly the overhead, which is the
        // bug this arithmetic exists to prevent.
        assert_eq!(MAX_DATAGRAM, 1200, "the wire format fixes this at 1200");
        assert_eq!(MAX_PAYLOAD + OVERHEAD, MAX_DATAGRAM);
        assert_eq!(MAX_PAYLOAD, 1160);
    }

    #[test]
    fn a_maximum_payload_still_fits_its_length_field() {
        // `payload_len` is a u16, so this is not close — but a future MTU that
        // did not fit would be a silent truncation rather than a refusal.
        assert!(MAX_PAYLOAD <= u16::MAX as usize);
    }

    use super::*;

    fn sample() -> Header {
        let mut h = Header::new(MsgType::Tick, [0xAB, 0xCD], [1, 2, 3, 4], 7, 123_456);
        h.payload_len = 32;
        h
    }

    #[test]
    fn header_round_trips_byte_for_byte() {
        let h = sample();
        let mut buf = [0u8; HEADER_LEN];
        assert_eq!(h.encode(&mut buf).unwrap(), HEADER_LEN);
        assert_eq!(Header::decode(&buf).unwrap(), h);
    }

    #[test]
    fn header_lands_on_the_documented_offsets() {
        let h = sample();
        let mut buf = [0u8; HEADER_LEN];
        h.encode(&mut buf).unwrap();

        assert_eq!(buf[0], MAGIC);
        assert_eq!(buf[1], 0x01, "major 0, minor 1 packed into one byte");
        assert_eq!(buf[2], MsgType::Tick.to_u8());
        assert_eq!(&buf[4..6], &[0xAB, 0xCD]);
        assert_eq!(&buf[6..10], &[1, 2, 3, 4]);
        assert_eq!(&buf[10..14], &7u32.to_le_bytes());
        assert_eq!(&buf[14..22], &123_456u64.to_le_bytes());
        assert_eq!(&buf[22..24], &32u16.to_le_bytes());
    }

    #[test]
    fn rejects_a_foreign_magic_byte() {
        let mut buf = [0u8; HEADER_LEN];
        sample().encode(&mut buf).unwrap();
        buf[0] = b'X';
        assert_eq!(Header::decode(&buf), Err(DecodeError::BadMagic(b'X')));
    }

    #[test]
    fn rejects_an_unknown_major_but_accepts_a_higher_minor() {
        let mut buf = [0u8; HEADER_LEN];
        sample().encode(&mut buf).unwrap();

        buf[1] = 0x10; // major 1
        assert_eq!(
            Header::decode(&buf),
            Err(DecodeError::UnsupportedVersion { major: 1, minor: 0 })
        );

        // A peer one minor version ahead must still parse: unknown types inside
        // it are ignored, which is what makes minor additions safe.
        buf[1] = 0x09; // major 0, minor 9
        assert_eq!(Header::decode(&buf).unwrap().version_minor, 9);
    }

    #[test]
    fn a_short_buffer_is_truncation() {
        let mut buf = [0u8; HEADER_LEN];
        sample().encode(&mut buf).unwrap();
        for n in 0..HEADER_LEN {
            assert_eq!(
                Header::decode(&buf[..n]),
                Err(DecodeError::Truncated),
                "prefix of {n} bytes should be truncated"
            );
        }
    }

    #[test]
    fn encoding_into_a_short_buffer_fails_rather_than_writing_partially() {
        let mut buf = [0u8; HEADER_LEN - 1];
        assert!(matches!(
            sample().encode(&mut buf),
            Err(EncodeError::BufferTooSmall { .. })
        ));
    }

    #[test]
    fn every_assigned_code_maps_both_ways() {
        let all = [
            MsgType::Hello,
            MsgType::Caps,
            MsgType::Get,
            MsgType::Set,
            MsgType::Tick,
            MsgType::SyncReq,
            MsgType::SyncResp,
            MsgType::Activate,
            MsgType::Chan,
            MsgType::ChanClaim,
            MsgType::ChanRelease,
            MsgType::Frame,
            MsgType::SrcPush,
            MsgType::SrcRenew,
            MsgType::SrcPop,
            MsgType::Event,
            MsgType::StateDigest,
            MsgType::StatePull,
            MsgType::StatePush,
            MsgType::ProgBegin,
            MsgType::ProgChunk,
            MsgType::ProgEnd,
            MsgType::FedHello,
            MsgType::FedEvent,
            MsgType::FedCue,
            MsgType::ProbeSet,
            MsgType::ProbeData,
            MsgType::TimeCtl,
        ];
        assert_eq!(all.len(), 28);
        for t in all {
            assert_eq!(MsgType::from_u8(t.to_u8()), Some(t));
        }
    }

    #[test]
    fn an_unknown_type_is_ignored_not_rejected() {
        // The whole forward-compatibility story rests on this: parsing a header
        // with a type we do not know must succeed, and only `typed()` says None.
        let mut h = sample();
        h.msg_type = 0x99;
        let mut buf = [0u8; HEADER_LEN];
        h.encode(&mut buf).unwrap();

        let decoded = Header::decode(&buf).unwrap();
        assert_eq!(decoded.msg_type, 0x99);
        assert_eq!(decoded.typed(), None);
    }

    #[test]
    fn vendor_codes_are_recognisable_as_such() {
        assert!(MsgType::is_vendor_code(0xF0));
        assert!(MsgType::is_vendor_code(0xFF));
        assert!(!MsgType::is_vendor_code(0xEF));
        assert_eq!(MsgType::from_u8(0xF0), None);
    }

    #[test]
    fn flags_compose_and_read_back() {
        let f = Flags::empty()
            .with(Flags::ENCRYPTED)
            .with(Flags::LAST_FRAGMENT);
        assert!(f.is_encrypted());
        assert!(f.is_last_fragment());
        assert!(!f.is_fragment());
        assert_eq!(f.0, 0b101);
        assert_eq!(Flags::default(), Flags::empty());
    }

    #[test]
    fn nonce_is_sender_then_sequence_then_boot_counter() {
        let h = sample();
        let n = h.nonce(0x0A0B0C0D);
        assert_eq!(&n[0..4], &[1, 2, 3, 4]);
        assert_eq!(&n[4..8], &7u32.to_le_bytes());
        assert_eq!(&n[8..12], &0x0A0B0C0Du32.to_le_bytes());
    }

    #[test]
    fn the_same_sequence_across_two_boots_gives_different_nonces() {
        // The reason boot_counter exists at all. If this ever fails, a rebooting
        // device reuses a nonce under the same key.
        let h = sample();
        assert_ne!(h.nonce(1), h.nonce(2));
    }

    #[test]
    fn overhead_is_forty_bytes() {
        assert_eq!(OVERHEAD, 40);
    }
}
