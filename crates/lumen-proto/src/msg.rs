//! Message payloads.
//!
//! Every payload type carries its own `decode`/`encode` pair against the shared
//! [`Reader`]/[`Writer`] cursors, and [`Payload`] dispatches on the header's type
//! byte. Adding a message means adding a struct and two match arms; nothing else
//! in the crate needs to know.
//!
//! Payloads borrow their buffer — `str`, `blob` and pixel data are slices into
//! the datagram, never copies. That is what keeps the codec allocation-free on a
//! device with no heap.

use crate::buf::{Reader, Writer};
use crate::error::{DecodeError, EncodeError};
use crate::header::MsgType;
use crate::Uuid;

type DResult<T> = Result<T, DecodeError>;
type EResult = Result<(), EncodeError>;

/// `TICK` — 0x10, multicast, 1 Hz.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tick {
    /// Repeated from the header so a `TICK` is self-contained in a log.
    pub show_time_us: u64,
    pub master_uuid: Uuid,
    pub master_capacity: u32,
    pub election_epoch: u32,
    /// 0 when the wall clock is unknown.
    pub wall_time_us: u64,
    pub wall_quality: WallQuality,
}

/// How much a receiver should trust `wall_time_us`.
///
/// This exists so schedules **degrade explicitly** when time is unknown, instead
/// of firing at a plausible-looking wrong moment.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum WallQuality {
    None = 0,
    AppSupplied = 1,
    Ntp = 2,
    GpsOrRtc = 3,
}

impl WallQuality {
    pub const fn from_u8(v: u8) -> Option<WallQuality> {
        Some(match v {
            0 => WallQuality::None,
            1 => WallQuality::AppSupplied,
            2 => WallQuality::Ntp,
            3 => WallQuality::GpsOrRtc,
            _ => return None,
        })
    }

    pub const fn to_u8(self) -> u8 {
        self as u8
    }
}

impl Tick {
    pub fn decode(r: &mut Reader<'_>) -> DResult<Tick> {
        let show_time_us = r.u64()?;
        let master_uuid = r.uuid()?;
        let master_capacity = r.u32()?;
        let election_epoch = r.u32()?;
        let wall_time_us = r.u64()?;
        let raw_quality = r.u8()?;
        let wall_quality = WallQuality::from_u8(raw_quality).ok_or(DecodeError::InvalidValue {
            field: "TICK.wall_quality",
        })?;
        r.skip(3)?; // reserved
        Ok(Tick {
            show_time_us,
            master_uuid,
            master_capacity,
            election_epoch,
            wall_time_us,
            wall_quality,
        })
    }

    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u64(self.show_time_us)?;
        w.uuid(&self.master_uuid)?;
        w.u32(self.master_capacity)?;
        w.u32(self.election_epoch)?;
        w.u64(self.wall_time_us)?;
        w.u8(self.wall_quality.to_u8())?;
        w.zeros(3)
    }
}

/// `SYNC_REQ` — 0x11.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SyncReq {
    pub t1: u64,
}

impl SyncReq {
    pub fn decode(r: &mut Reader<'_>) -> DResult<SyncReq> {
        Ok(SyncReq { t1: r.u64()? })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u64(self.t1)
    }
}

/// `SYNC_RESP` — 0x12.
///
/// `t4` is recorded locally by the requester and never travels. Offset is
/// `((t2-t1)+(t3-t4))/2`; RTT is `(t4-t1)-(t3-t2)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SyncResp {
    /// Echoed from the request.
    pub t1: u64,
    pub t2: u64,
    pub t3: u64,
}

impl SyncResp {
    pub fn decode(r: &mut Reader<'_>) -> DResult<SyncResp> {
        Ok(SyncResp {
            t1: r.u64()?,
            t2: r.u64()?,
            t3: r.u64()?,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u64(self.t1)?;
        w.u64(self.t2)?;
        w.u64(self.t3)
    }
}

/// Sentinel `slot` meaning "device chooses a free slot".
///
/// This is what a controller should normally send, rather than guessing at
/// another device's memory: pool size varies by device and is reported in
/// `CAPS`.
pub const SLOT_DEVICE_CHOOSES: u8 = 0xFF;

/// `ACTIVATE` — 0x20.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Activate {
    pub program_id: u16,
    /// Index into the device's program pool, not one of a fixed pair.
    pub slot: u8,
    /// Show time at which this takes effect.
    ///
    /// Scheduled activation, never "go now" — so the mesh switches together even
    /// across a network hiccup.
    pub activate_at: u64,
}

impl Activate {
    pub fn decode(r: &mut Reader<'_>) -> DResult<Activate> {
        let program_id = r.u16()?;
        let slot = r.u8()?;
        r.skip(1)?; // reserved
        let activate_at = r.u64()?;
        Ok(Activate {
            program_id,
            slot,
            activate_at,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u16(self.program_id)?;
        w.u8(self.slot)?;
        w.zeros(1)?;
        w.u64(self.activate_at)
    }
}

/// `CHAN` — 0x21. Latest-wins with hold.
///
/// A receiver drops any `CHAN` whose `producer_seq` is older than the newest seen
/// from the current owner.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Chan<'a> {
    pub channel_id: u16,
    pub producer_seq: u16,
    pub payload: &'a [u8],
}

impl<'a> Chan<'a> {
    pub fn decode(r: &mut Reader<'a>) -> DResult<Chan<'a>> {
        Ok(Chan {
            channel_id: r.u16()?,
            producer_seq: r.u16()?,
            payload: r.blob()?,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u16(self.channel_id)?;
        w.u16(self.producer_seq)?;
        w.blob(self.payload)
    }
}

/// `CHAN_CLAIM` — 0x22.
///
/// Strictly-greater priority preempts; equal priority does not, so two identical
/// producers never fight.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChanClaim {
    pub channel_id: u16,
    pub priority: u8,
    pub lease_ms: u32,
}

impl ChanClaim {
    pub fn decode(r: &mut Reader<'_>) -> DResult<ChanClaim> {
        let channel_id = r.u16()?;
        let priority = r.u8()?;
        r.skip(1)?; // reserved
        let lease_ms = r.u32()?;
        Ok(ChanClaim {
            channel_id,
            priority,
            lease_ms,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u16(self.channel_id)?;
        w.u8(self.priority)?;
        w.zeros(1)?;
        w.u32(self.lease_ms)
    }
}

/// `CHAN_RELEASE` — 0x23.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChanRelease {
    pub channel_id: u16,
}

impl ChanRelease {
    pub fn decode(r: &mut Reader<'_>) -> DResult<ChanRelease> {
        let channel_id = r.u16()?;
        r.skip(2)?; // reserved
        Ok(ChanRelease { channel_id })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u16(self.channel_id)?;
        w.zeros(2)
    }
}

/// Pixel encoding used by a `FRAME`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PixelFormat {
    Rgb8 = 0,
    Rgbw8 = 1,
    Rgb16 = 2,
    Cct = 3,
}

impl PixelFormat {
    pub const fn from_u8(v: u8) -> Option<PixelFormat> {
        Some(match v {
            0 => PixelFormat::Rgb8,
            1 => PixelFormat::Rgbw8,
            2 => PixelFormat::Rgb16,
            3 => PixelFormat::Cct,
            _ => return None,
        })
    }

    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Bytes each pixel occupies in this format.
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgb8 => 3,
            PixelFormat::Rgbw8 => 4,
            PixelFormat::Rgb16 => 6,
            // Correlated colour temperature plus intensity.
            PixelFormat::Cct => 4,
        }
    }
}

/// `FRAME` — 0x24. Direct pixel data, bypassing the VM.
///
/// Fragmentation uses the header `flags` bits when a segment exceeds the MTU.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Frame<'a> {
    pub segment_id: u16,
    /// Index of the first pixel this frame carries.
    pub offset: u16,
    pub format: PixelFormat,
    pub priority: u8,
    pub count: u16,
    /// Exactly `count * format.bytes_per_pixel()` bytes.
    pub pixels: &'a [u8],
}

impl<'a> Frame<'a> {
    pub fn decode(r: &mut Reader<'a>) -> DResult<Frame<'a>> {
        let segment_id = r.u16()?;
        let offset = r.u16()?;
        let raw_format = r.u8()?;
        let format = PixelFormat::from_u8(raw_format).ok_or(DecodeError::InvalidValue {
            field: "FRAME.format",
        })?;
        let priority = r.u8()?;
        let count = r.u16()?;
        let need = count as usize * format.bytes_per_pixel();
        let pixels = r.bytes(need)?;
        Ok(Frame {
            segment_id,
            offset,
            format,
            priority,
            count,
            pixels,
        })
    }

    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        let need = self.count as usize * self.format.bytes_per_pixel();
        if self.pixels.len() != need {
            return Err(EncodeError::Invalid(DecodeError::InvalidValue {
                field: "FRAME.pixels",
            }));
        }
        w.u16(self.segment_id)?;
        w.u16(self.offset)?;
        w.u8(self.format.to_u8())?;
        w.u8(self.priority)?;
        w.u16(self.count)?;
        w.bytes(self.pixels)
    }
}

/// The highest priority that may omit an expiry.
///
/// The ambient band is 0–63, and that band **is** the floor: a floor that had to
/// expire would not be a floor, because something has to hold the lights when
/// every show, override and alert has gone.
///
/// Earlier this was 0, which contradicted the band table in the runtime model
/// and split the two implementations — this codec refused an ambient scene at
/// priority 40 that the source stack accepted.
pub const AMBIENT_FLOOR_PRIORITY: u8 = 63;

/// `SRC_PUSH` — 0x30. Push a source onto a zone's stack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SrcPush<'a> {
    pub source_id: Uuid,
    pub zone_id: Uuid,
    pub scene_id: Uuid,
    pub priority: u8,
    pub fade_in_ms: u16,
    pub fade_out_ms: u16,
    /// Absolute show time, not a duration — every device shares that clock, so a
    /// source expires at the same instant everywhere regardless of when each
    /// device received the push.
    ///
    /// `None` is legal **only** at the ambient floor.
    pub expires_at: Option<u64>,
    pub param_overrides: &'a [u8],
}

impl SrcPush<'_> {
    const FLAG_HAS_EXPIRY: u8 = 1 << 0;

    /// The "stuck red at 3am" rule, enforced at the wire level: a source above
    /// the ambient floor with no expiry is refused, so no client can create the
    /// condition even by accident.
    fn check_expiry(priority: u8, expires_at: Option<u64>) -> DResult<()> {
        if priority > AMBIENT_FLOOR_PRIORITY && expires_at.is_none() {
            return Err(DecodeError::SourceWithoutExpiry { priority });
        }
        Ok(())
    }
}

impl<'a> SrcPush<'a> {
    pub fn decode(r: &mut Reader<'a>) -> DResult<SrcPush<'a>> {
        let source_id = r.uuid()?;
        let zone_id = r.uuid()?;
        let scene_id = r.uuid()?;
        let priority = r.u8()?;
        let flags = r.u8()?;
        let fade_in_ms = r.u16()?;
        let fade_out_ms = r.u16()?;
        r.skip(2)?; // reserved
        let expires_at = if flags & Self::FLAG_HAS_EXPIRY != 0 {
            Some(r.u64()?)
        } else {
            None
        };
        let param_overrides = r.blob()?;

        Self::check_expiry(priority, expires_at)?;

        Ok(SrcPush {
            source_id,
            zone_id,
            scene_id,
            priority,
            fade_in_ms,
            fade_out_ms,
            expires_at,
            param_overrides,
        })
    }

    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        Self::check_expiry(self.priority, self.expires_at)?;
        w.uuid(&self.source_id)?;
        w.uuid(&self.zone_id)?;
        w.uuid(&self.scene_id)?;
        w.u8(self.priority)?;
        w.u8(if self.expires_at.is_some() {
            Self::FLAG_HAS_EXPIRY
        } else {
            0
        })?;
        w.u16(self.fade_in_ms)?;
        w.u16(self.fade_out_ms)?;
        w.zeros(2)?;
        if let Some(t) = self.expires_at {
            w.u64(t)?;
        }
        w.blob(self.param_overrides)
    }
}

/// `SRC_RENEW` — 0x31.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SrcRenew {
    pub source_id: Uuid,
    pub expires_at: u64,
}

impl SrcRenew {
    pub fn decode(r: &mut Reader<'_>) -> DResult<SrcRenew> {
        Ok(SrcRenew {
            source_id: r.uuid()?,
            expires_at: r.u64()?,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.uuid(&self.source_id)?;
        w.u64(self.expires_at)
    }
}

/// `SRC_POP` — 0x32.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SrcPop {
    pub source_id: Uuid,
    pub fade_out_ms: u16,
}

impl SrcPop {
    pub fn decode(r: &mut Reader<'_>) -> DResult<SrcPop> {
        Ok(SrcPop {
            source_id: r.uuid()?,
            fade_out_ms: r.u16()?,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.uuid(&self.source_id)?;
        w.u16(self.fade_out_ms)
    }
}

/// `EVENT` — 0x40.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Event<'a> {
    /// Minted by the producer, never derived by a receiver.
    ///
    /// That is what lets every keeper compute the same action id and collapse
    /// duplicate outbound calls.
    pub event_id: Uuid,
    pub source_uuid: Uuid,
    pub kind: &'a str,
    /// Q16.16.
    pub value: i32,
    /// 0 when unknown.
    pub wall_time_us: u64,
}

impl<'a> Event<'a> {
    pub fn decode(r: &mut Reader<'a>) -> DResult<Event<'a>> {
        Ok(Event {
            event_id: r.uuid()?,
            source_uuid: r.uuid()?,
            kind: r.str()?,
            value: r.q16()?,
            wall_time_us: r.u64()?,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.uuid(&self.event_id)?;
        w.uuid(&self.source_uuid)?;
        w.str(self.kind)?;
        w.q16(self.value)?;
        w.u64(self.wall_time_us)
    }
}

/// One `(record_id, hlc)` pair in a `STATE_DIGEST`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DigestEntry {
    pub record_id: Uuid,
    /// Hybrid logical clock.
    pub hlc: u64,
}

/// `STATE_DIGEST` — 0x50.
///
/// Entries stay as borrowed bytes and are handed out by [`Self::entries`]. With
/// no heap there is nowhere to collect them, and gossip digests are walked once
/// anyway.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StateDigest<'a> {
    pub count: u16,
    body: &'a [u8],
}

impl<'a> StateDigest<'a> {
    const ENTRY_LEN: usize = 24;

    pub fn decode(r: &mut Reader<'a>) -> DResult<StateDigest<'a>> {
        let count = r.u16()?;
        let body = r.bytes(count as usize * Self::ENTRY_LEN)?;
        Ok(StateDigest { count, body })
    }

    /// Walk the entries.
    pub fn entries(&self) -> impl Iterator<Item = DigestEntry> + 'a {
        let mut r = Reader::new(self.body);
        core::iter::from_fn(move || {
            if r.remaining() < Self::ENTRY_LEN {
                return None;
            }
            let record_id = r.uuid().ok()?;
            let hlc = r.u64().ok()?;
            Some(DigestEntry { record_id, hlc })
        })
    }

    /// Build one from a slice of entries, writing straight into `w`.
    pub fn encode_from(entries: &[DigestEntry], w: &mut Writer<'_>) -> EResult {
        if entries.len() > u16::MAX as usize {
            return Err(EncodeError::Invalid(DecodeError::InvalidValue {
                field: "STATE_DIGEST.count",
            }));
        }
        w.u16(entries.len() as u16)?;
        for e in entries {
            w.uuid(&e.record_id)?;
            w.u64(e.hlc)?;
        }
        Ok(())
    }

    /// Re-emit exactly the bytes this was decoded from.
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u16(self.count)?;
        w.bytes(self.body)
    }
}

/// `STATE_PULL` — 0x51. A list of record ids the sender wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StatePull<'a> {
    pub count: u16,
    body: &'a [u8],
}

impl<'a> StatePull<'a> {
    pub fn decode(r: &mut Reader<'a>) -> DResult<StatePull<'a>> {
        let count = r.u16()?;
        let body = r.bytes(count as usize * 16)?;
        Ok(StatePull { count, body })
    }

    pub fn ids(&self) -> impl Iterator<Item = Uuid> + 'a {
        let mut r = Reader::new(self.body);
        core::iter::from_fn(move || r.uuid().ok())
    }

    pub fn encode_from(ids: &[Uuid], w: &mut Writer<'_>) -> EResult {
        if ids.len() > u16::MAX as usize {
            return Err(EncodeError::Invalid(DecodeError::InvalidValue {
                field: "STATE_PULL.count",
            }));
        }
        w.u16(ids.len() as u16)?;
        for id in ids {
            w.uuid(id)?;
        }
        Ok(())
    }

    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u16(self.count)?;
        w.bytes(self.body)
    }
}

/// One signed record inside a `STATE_PUSH`.
///
/// The signature covers `record_id ‖ record_type ‖ hlc ‖ author ‖ body`, in that
/// order. [`Self::signed_bytes_into`] is the single place that ordering is
/// written down, so a verifier and a signer cannot disagree about it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StateRecord<'a> {
    pub record_id: Uuid,
    pub record_type: u8,
    pub hlc: u64,
    pub author: Uuid,
    pub body: &'a [u8],
    pub sig: &'a [u8; 64],
}

impl StateRecord<'_> {
    /// Serialise exactly the bytes the signature covers.
    ///
    /// Returns the number written. Verify only on change — the digest exchange
    /// compares HLCs first, so steady-state gossip costs no signature checks.
    pub fn signed_bytes_into(&self, out: &mut [u8]) -> Result<usize, EncodeError> {
        let mut w = Writer::new(out);
        w.uuid(&self.record_id)?;
        w.u8(self.record_type)?;
        w.u64(self.hlc)?;
        w.uuid(&self.author)?;
        w.bytes(self.body)?;
        Ok(w.position())
    }

    /// Bytes [`Self::signed_bytes_into`] will write, so a caller can size a
    /// buffer without a trial run.
    pub const fn signed_len(&self) -> usize {
        16 + 1 + 8 + 16 + self.body.len()
    }
}

/// `STATE_PUSH` — 0x52.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StatePush<'a> {
    pub count: u16,
    body: &'a [u8],
}

impl<'a> StatePush<'a> {
    pub fn decode(r: &mut Reader<'a>) -> DResult<StatePush<'a>> {
        let count = r.u16()?;
        // Records are variable length, so walk them once to find where the
        // message ends rather than trusting a length that is not on the wire.
        let start = r.rest();
        let mut probe = Reader::new(start);
        for _ in 0..count {
            probe.skip(16)?; // record_id
            probe.skip(1)?; // record_type
            probe.skip(8)?; // hlc
            probe.skip(16)?; // author
            let len = probe.u16()? as usize;
            probe.skip(len)?; // body
            probe.skip(64)?; // sig
        }
        let taken = probe.position();
        let body = r.bytes(taken)?;
        Ok(StatePush { count, body })
    }

    /// Walk the records. Stops at the first malformed one, which cannot happen
    /// for a value produced by [`Self::decode`].
    pub fn records(&self) -> impl Iterator<Item = StateRecord<'a>> + 'a {
        let mut r = Reader::new(self.body);
        core::iter::from_fn(move || {
            if r.is_empty() {
                return None;
            }
            let record_id = r.uuid().ok()?;
            let record_type = r.u8().ok()?;
            let hlc = r.u64().ok()?;
            let author = r.uuid().ok()?;
            let body = r.blob().ok()?;
            let sig_bytes = r.bytes(64).ok()?;
            let sig: &[u8; 64] = sig_bytes.try_into().ok()?;
            Some(StateRecord {
                record_id,
                record_type,
                hlc,
                author,
                body,
                sig,
            })
        })
    }

    pub fn encode_from(records: &[StateRecord<'_>], w: &mut Writer<'_>) -> EResult {
        if records.len() > u16::MAX as usize {
            return Err(EncodeError::Invalid(DecodeError::InvalidValue {
                field: "STATE_PUSH.count",
            }));
        }
        w.u16(records.len() as u16)?;
        for rec in records {
            w.uuid(&rec.record_id)?;
            w.u8(rec.record_type)?;
            w.u64(rec.hlc)?;
            w.uuid(&rec.author)?;
            w.blob(rec.body)?;
            w.bytes(rec.sig)?;
        }
        Ok(())
    }

    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u16(self.count)?;
        w.bytes(self.body)
    }
}

/// `PROG_BEGIN` — 0x60.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProgBegin<'a> {
    pub program_id: u16,
    /// [`SLOT_DEVICE_CHOOSES`] asks the device to pick.
    pub slot: u8,
    /// The **minimum** VM version this program needs.
    ///
    /// Instructions are append-only within a VM major version, so a device
    /// refuses only programs needing more than it implements — a firmware
    /// upgrade never invalidates a program already running.
    pub vm_min_version: u8,
    pub total_len: u32,
    pub device_class: &'a str,
}

impl<'a> ProgBegin<'a> {
    pub fn decode(r: &mut Reader<'a>) -> DResult<ProgBegin<'a>> {
        Ok(ProgBegin {
            program_id: r.u16()?,
            slot: r.u8()?,
            vm_min_version: r.u8()?,
            total_len: r.u32()?,
            device_class: r.str()?,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u16(self.program_id)?;
        w.u8(self.slot)?;
        w.u8(self.vm_min_version)?;
        w.u32(self.total_len)?;
        w.str(self.device_class)
    }
}

/// `PROG_CHUNK` — 0x61.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProgChunk<'a> {
    pub program_id: u16,
    pub offset: u32,
    pub data: &'a [u8],
}

impl<'a> ProgChunk<'a> {
    pub fn decode(r: &mut Reader<'a>) -> DResult<ProgChunk<'a>> {
        Ok(ProgChunk {
            program_id: r.u16()?,
            offset: r.u32()?,
            data: r.blob()?,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u16(self.program_id)?;
        w.u32(self.offset)?;
        w.blob(self.data)
    }
}

/// `PROG_END` — 0x62.
///
/// The slot is valid only if the hash **and** the signature verify: the hash
/// proves the transfer was clean, the signature proves who sent it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProgEnd {
    pub program_id: u16,
    pub sha256: [u8; 32],
    pub sig: [u8; 64],
}

impl ProgEnd {
    pub fn decode(r: &mut Reader<'_>) -> DResult<ProgEnd> {
        Ok(ProgEnd {
            program_id: r.u16()?,
            sha256: r.array()?,
            sig: r.array()?,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u16(self.program_id)?;
        w.bytes(&self.sha256)?;
        w.bytes(&self.sig)
    }
}

/// `FED_HELLO` — 0x70, cross-mesh.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FedHello<'a> {
    pub mesh_id: Uuid,
    pub mesh_name: &'a str,
    pub caps: u32,
    pub fed_pubkey: [u8; 32],
}

impl<'a> FedHello<'a> {
    pub fn decode(r: &mut Reader<'a>) -> DResult<FedHello<'a>> {
        Ok(FedHello {
            mesh_id: r.uuid()?,
            mesh_name: r.str()?,
            caps: r.u32()?,
            fed_pubkey: r.array()?,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.uuid(&self.mesh_id)?;
        w.str(self.mesh_name)?;
        w.u32(self.caps)?;
        w.bytes(&self.fed_pubkey)
    }
}

/// `FED_EVENT` — 0x71. An [`Event`] plus the mesh it came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FedEvent<'a> {
    pub event: Event<'a>,
    pub origin_mesh: Uuid,
}

impl<'a> FedEvent<'a> {
    pub fn decode(r: &mut Reader<'a>) -> DResult<FedEvent<'a>> {
        Ok(FedEvent {
            event: Event::decode(r)?,
            origin_mesh: r.uuid()?,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        self.event.encode(w)?;
        w.uuid(&self.origin_mesh)
    }
}

/// `FED_CUE` — 0x72.
///
/// Scheduled against **wall time**, not show time: federated meshes have
/// independent timebases, so this is coarse by construction and the field name
/// says so.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FedCue<'a> {
    pub cue_name: &'a str,
    pub wall_at_us: u64,
    pub origin_mesh: Uuid,
}

impl<'a> FedCue<'a> {
    pub fn decode(r: &mut Reader<'a>) -> DResult<FedCue<'a>> {
        Ok(FedCue {
            cue_name: r.str()?,
            wall_at_us: r.u64()?,
            origin_mesh: r.uuid()?,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.str(self.cue_name)?;
        w.u64(self.wall_at_us)?;
        w.uuid(&self.origin_mesh)
    }
}

/// `PROBE_SET` — 0x80.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProbeSet {
    pub program_id: u16,
    pub probe_id: u16,
    pub pixel_index: u16,
}

impl ProbeSet {
    pub fn decode(r: &mut Reader<'_>) -> DResult<ProbeSet> {
        let program_id = r.u16()?;
        let probe_id = r.u16()?;
        let pixel_index = r.u16()?;
        r.skip(2)?; // reserved
        Ok(ProbeSet {
            program_id,
            probe_id,
            pixel_index,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u16(self.program_id)?;
        w.u16(self.probe_id)?;
        w.u16(self.pixel_index)?;
        w.zeros(2)
    }
}

/// `PROBE_DATA` — 0x81.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProbeData {
    pub probe_id: u16,
    pub pixel_index: u16,
    pub frame_show_time: u64,
    /// Q16.16.
    pub value: i32,
}

impl ProbeData {
    pub fn decode(r: &mut Reader<'_>) -> DResult<ProbeData> {
        Ok(ProbeData {
            probe_id: r.u16()?,
            pixel_index: r.u16()?,
            frame_show_time: r.u64()?,
            value: r.q16()?,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u16(self.probe_id)?;
        w.u16(self.pixel_index)?;
        w.u64(self.frame_show_time)?;
        w.q16(self.value)
    }
}

/// What a `TIMECTL` asks a device to do with its show clock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TimeMode {
    Run = 0,
    Pause = 1,
    Step = 2,
    Set = 3,
}

impl TimeMode {
    pub const fn from_u8(v: u8) -> Option<TimeMode> {
        Some(match v {
            0 => TimeMode::Run,
            1 => TimeMode::Pause,
            2 => TimeMode::Step,
            3 => TimeMode::Set,
            _ => return None,
        })
    }
    pub const fn to_u8(self) -> u8 {
        self as u8
    }
}

/// `TIMECTL` — 0x82.
///
/// Carries a lease so a crashed editor cannot leave a mesh frozen; on lapse a
/// device resumes free-running.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TimeCtl {
    pub mode: TimeMode,
    pub lease_ms: u32,
    pub target_show_time: u64,
}

impl TimeCtl {
    pub fn decode(r: &mut Reader<'_>) -> DResult<TimeCtl> {
        let raw = r.u8()?;
        let mode = TimeMode::from_u8(raw).ok_or(DecodeError::InvalidValue {
            field: "TIMECTL.mode",
        })?;
        r.skip(3)?; // reserved
        Ok(TimeCtl {
            mode,
            lease_ms: r.u32()?,
            target_show_time: r.u64()?,
        })
    }
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.u8(self.mode.to_u8())?;
        w.zeros(3)?;
        w.u32(self.lease_ms)?;
        w.u64(self.target_show_time)
    }
}

/// A decoded payload.
///
/// [`Payload::Unparsed`] carries the body of a message whose layout the spec has
/// not fixed yet — `HELLO`, `CAPS`, `GET` and `SET` are named in the type table
/// but have no byte layout in the wire format. Inventing one here would put a
/// guess on the wire under a normative name, so it stays raw until the spec says
/// otherwise.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Payload<'a> {
    Tick(Tick),
    SyncReq(SyncReq),
    SyncResp(SyncResp),
    Activate(Activate),
    Chan(Chan<'a>),
    ChanClaim(ChanClaim),
    ChanRelease(ChanRelease),
    Frame(Frame<'a>),
    SrcPush(SrcPush<'a>),
    SrcRenew(SrcRenew),
    SrcPop(SrcPop),
    Event(Event<'a>),
    StateDigest(StateDigest<'a>),
    StatePull(StatePull<'a>),
    StatePush(StatePush<'a>),
    ProgBegin(ProgBegin<'a>),
    ProgChunk(ProgChunk<'a>),
    ProgEnd(ProgEnd),
    FedHello(FedHello<'a>),
    FedEvent(FedEvent<'a>),
    FedCue(FedCue<'a>),
    ProbeSet(ProbeSet),
    ProbeData(ProbeData),
    TimeCtl(TimeCtl),
    /// A known type whose payload layout the spec has not fixed yet.
    Unparsed {
        msg_type: MsgType,
        body: &'a [u8],
    },
}

impl<'a> Payload<'a> {
    /// Decode `body` according to `msg_type`.
    ///
    /// Trailing bytes are **not** an error: a peer one minor version ahead may
    /// have appended a field, and refusing that would defeat forward
    /// compatibility.
    pub fn decode(msg_type: MsgType, body: &'a [u8]) -> DResult<Payload<'a>> {
        let mut r = Reader::new(body);
        Ok(match msg_type {
            MsgType::Tick => Payload::Tick(Tick::decode(&mut r)?),
            MsgType::SyncReq => Payload::SyncReq(SyncReq::decode(&mut r)?),
            MsgType::SyncResp => Payload::SyncResp(SyncResp::decode(&mut r)?),
            MsgType::Activate => Payload::Activate(Activate::decode(&mut r)?),
            MsgType::Chan => Payload::Chan(Chan::decode(&mut r)?),
            MsgType::ChanClaim => Payload::ChanClaim(ChanClaim::decode(&mut r)?),
            MsgType::ChanRelease => Payload::ChanRelease(ChanRelease::decode(&mut r)?),
            MsgType::Frame => Payload::Frame(Frame::decode(&mut r)?),
            MsgType::SrcPush => Payload::SrcPush(SrcPush::decode(&mut r)?),
            MsgType::SrcRenew => Payload::SrcRenew(SrcRenew::decode(&mut r)?),
            MsgType::SrcPop => Payload::SrcPop(SrcPop::decode(&mut r)?),
            MsgType::Event => Payload::Event(Event::decode(&mut r)?),
            MsgType::StateDigest => Payload::StateDigest(StateDigest::decode(&mut r)?),
            MsgType::StatePull => Payload::StatePull(StatePull::decode(&mut r)?),
            MsgType::StatePush => Payload::StatePush(StatePush::decode(&mut r)?),
            MsgType::ProgBegin => Payload::ProgBegin(ProgBegin::decode(&mut r)?),
            MsgType::ProgChunk => Payload::ProgChunk(ProgChunk::decode(&mut r)?),
            MsgType::ProgEnd => Payload::ProgEnd(ProgEnd::decode(&mut r)?),
            MsgType::FedHello => Payload::FedHello(FedHello::decode(&mut r)?),
            MsgType::FedEvent => Payload::FedEvent(FedEvent::decode(&mut r)?),
            MsgType::FedCue => Payload::FedCue(FedCue::decode(&mut r)?),
            MsgType::ProbeSet => Payload::ProbeSet(ProbeSet::decode(&mut r)?),
            MsgType::ProbeData => Payload::ProbeData(ProbeData::decode(&mut r)?),
            MsgType::TimeCtl => Payload::TimeCtl(TimeCtl::decode(&mut r)?),
            MsgType::Hello | MsgType::Caps | MsgType::Get | MsgType::Set => {
                Payload::Unparsed { msg_type, body }
            }
        })
    }

    /// The message type this payload belongs to.
    pub const fn msg_type(&self) -> MsgType {
        match self {
            Payload::Tick(_) => MsgType::Tick,
            Payload::SyncReq(_) => MsgType::SyncReq,
            Payload::SyncResp(_) => MsgType::SyncResp,
            Payload::Activate(_) => MsgType::Activate,
            Payload::Chan(_) => MsgType::Chan,
            Payload::ChanClaim(_) => MsgType::ChanClaim,
            Payload::ChanRelease(_) => MsgType::ChanRelease,
            Payload::Frame(_) => MsgType::Frame,
            Payload::SrcPush(_) => MsgType::SrcPush,
            Payload::SrcRenew(_) => MsgType::SrcRenew,
            Payload::SrcPop(_) => MsgType::SrcPop,
            Payload::Event(_) => MsgType::Event,
            Payload::StateDigest(_) => MsgType::StateDigest,
            Payload::StatePull(_) => MsgType::StatePull,
            Payload::StatePush(_) => MsgType::StatePush,
            Payload::ProgBegin(_) => MsgType::ProgBegin,
            Payload::ProgChunk(_) => MsgType::ProgChunk,
            Payload::ProgEnd(_) => MsgType::ProgEnd,
            Payload::FedHello(_) => MsgType::FedHello,
            Payload::FedEvent(_) => MsgType::FedEvent,
            Payload::FedCue(_) => MsgType::FedCue,
            Payload::ProbeSet(_) => MsgType::ProbeSet,
            Payload::ProbeData(_) => MsgType::ProbeData,
            Payload::TimeCtl(_) => MsgType::TimeCtl,
            Payload::Unparsed { msg_type, .. } => *msg_type,
        }
    }

    /// Encode into `w`.
    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        match self {
            Payload::Tick(m) => m.encode(w),
            Payload::SyncReq(m) => m.encode(w),
            Payload::SyncResp(m) => m.encode(w),
            Payload::Activate(m) => m.encode(w),
            Payload::Chan(m) => m.encode(w),
            Payload::ChanClaim(m) => m.encode(w),
            Payload::ChanRelease(m) => m.encode(w),
            Payload::Frame(m) => m.encode(w),
            Payload::SrcPush(m) => m.encode(w),
            Payload::SrcRenew(m) => m.encode(w),
            Payload::SrcPop(m) => m.encode(w),
            Payload::Event(m) => m.encode(w),
            Payload::StateDigest(m) => m.encode(w),
            Payload::StatePull(m) => m.encode(w),
            Payload::StatePush(m) => m.encode(w),
            Payload::ProgBegin(m) => m.encode(w),
            Payload::ProgChunk(m) => m.encode(w),
            Payload::ProgEnd(m) => m.encode(w),
            Payload::FedHello(m) => m.encode(w),
            Payload::FedEvent(m) => m.encode(w),
            Payload::FedCue(m) => m.encode(w),
            Payload::ProbeSet(m) => m.encode(w),
            Payload::ProbeData(m) => m.encode(w),
            Payload::TimeCtl(m) => m.encode(w),
            Payload::Unparsed { body, .. } => w.bytes(body),
        }
    }
}
