//! Codec round-trips and rejection cases, driven through the public API only.
//!
//! Written as an integration test on purpose: if something here needs a private
//! item, the crate is not usable by the third-party controllers it exists to
//! serve.
//!
//! The rejection half matters more than the round-trip half. A naive
//! implementation accepts everything and looks perfectly healthy right up until
//! something malformed reaches it, so every "must be refused" rule in the wire
//! format gets a test here.

use lumen_proto::buf::{Reader, Writer};
use lumen_proto::msg::*;
use lumen_proto::{DecodeError, EncodeError, MsgType, Payload, Uuid};

fn uuid(n: u8) -> Uuid {
    Uuid([n; 16])
}

/// Encode a payload, decode it back, and assert the value survives unchanged.
fn assert_round_trips(p: &Payload<'_>) {
    let mut scratch = [0u8; 2048];
    let n = {
        let mut w = Writer::new(&mut scratch);
        p.encode(&mut w).expect("encode");
        w.position()
    };
    let decoded = Payload::decode(p.msg_type(), &scratch[..n]).expect("decode");
    assert_eq!(&decoded, p, "round trip changed the value");

    // And re-encoding the decoded form must reproduce identical bytes. A codec
    // that only round-trips one way lets an encoder and a decoder drift together.
    let mut again = [0u8; 2048];
    let m = {
        let mut w = Writer::new(&mut again);
        decoded.encode(&mut w).expect("re-encode");
        w.position()
    };
    assert_eq!(&again[..m], &scratch[..n], "re-encode differed");
}

/// Every prefix of a valid encoding must be refused cleanly rather than panic.
/// Fuzzing in miniature: a panic here would be a remote denial of service.
fn assert_no_prefix_panics(p: &Payload<'_>) {
    let mut scratch = [0u8; 2048];
    let n = {
        let mut w = Writer::new(&mut scratch);
        p.encode(&mut w).expect("encode");
        w.position()
    };
    for len in 0..n {
        let _ = Payload::decode(p.msg_type(), &scratch[..len]);
    }
}

fn sample_payloads() -> [Payload<'static>; 20] {
    static PIXELS: [u8; 6] = [1, 2, 3, 4, 5, 6];
    static PARAMS: [u8; 3] = [7, 7, 7];
    static CHUNK: [u8; 4] = [0xAB, 0xCD, 0xEF, 0x01];
    static CHANDATA: [u8; 5] = [9, 8, 7, 6, 5];

    [
        Payload::Tick(Tick {
            show_time_us: 1_000_000,
            master_uuid: uuid(1),
            master_capacity: 4242,
            election_epoch: 7,
            wall_time_us: 1_700_000_000_000_000,
            wall_quality: WallQuality::Ntp,
        }),
        Payload::SyncReq(SyncReq { t1: 42 }),
        Payload::SyncResp(SyncResp {
            t1: 1,
            t2: 2,
            t3: 3,
        }),
        Payload::Activate(Activate {
            program_id: 9,
            slot: SLOT_DEVICE_CHOOSES,
            activate_at: 5_000_000,
        }),
        Payload::Chan(Chan {
            channel_id: 3,
            producer_seq: 11,
            payload: &CHANDATA,
        }),
        Payload::ChanClaim(ChanClaim {
            channel_id: 3,
            priority: 200,
            lease_ms: 5000,
        }),
        Payload::ChanRelease(ChanRelease { channel_id: 3 }),
        Payload::Frame(Frame {
            segment_id: 1,
            offset: 10,
            format: PixelFormat::Rgb8,
            priority: 100,
            count: 2,
            pixels: &PIXELS,
        }),
        Payload::SrcPush(SrcPush {
            source_id: uuid(4),
            zone_id: uuid(5),
            scene_id: uuid(6),
            priority: 200,
            fade_in_ms: 250,
            fade_out_ms: 750,
            expires_at: Some(9_000_000),
            param_overrides: &PARAMS,
        }),
        Payload::SrcRenew(SrcRenew {
            source_id: uuid(1),
            expires_at: 123,
        }),
        Payload::SrcPop(SrcPop {
            source_id: uuid(1),
            fade_out_ms: 400,
        }),
        Payload::Event(Event {
            event_id: uuid(1),
            source_uuid: uuid(2),
            kind: "motion",
            value: -32768,
            wall_time_us: 0,
        }),
        Payload::ProgBegin(ProgBegin {
            program_id: 1,
            slot: SLOT_DEVICE_CHOOSES,
            vm_min_version: 2,
            total_len: 4096,
            device_class: "esp32c6",
        }),
        Payload::ProgChunk(ProgChunk {
            program_id: 1,
            offset: 512,
            data: &CHUNK,
        }),
        Payload::ProgEnd(ProgEnd {
            program_id: 1,
            sha256: [3u8; 32],
            sig: [4u8; 64],
        }),
        Payload::FedHello(FedHello {
            mesh_id: uuid(1),
            mesh_name: "workshop",
            caps: 0b1011,
            fed_pubkey: [5u8; 32],
        }),
        Payload::FedEvent(FedEvent {
            event: Event {
                event_id: uuid(1),
                source_uuid: uuid(2),
                kind: "sunset",
                value: 65536,
                wall_time_us: 1_700_000_000_000_000,
            },
            origin_mesh: uuid(3),
        }),
        Payload::FedCue(FedCue {
            cue_name: "act-two",
            wall_at_us: 1_700_000_000_000_000,
            origin_mesh: uuid(3),
        }),
        Payload::ProbeSet(ProbeSet {
            program_id: 1,
            probe_id: 2,
            pixel_index: 3,
        }),
        Payload::ProbeData(ProbeData {
            probe_id: 2,
            pixel_index: 3,
            frame_show_time: 4,
            value: -1,
        }),
    ]
}

#[test]
fn every_sample_payload_round_trips_byte_for_byte() {
    for p in sample_payloads() {
        assert_round_trips(&p);
    }
}

#[test]
fn no_truncated_payload_panics() {
    for p in sample_payloads() {
        assert_no_prefix_panics(&p);
    }
}

#[test]
fn msg_type_is_reported_for_every_variant() {
    let expected = [
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
        MsgType::ProgBegin,
        MsgType::ProgChunk,
        MsgType::ProgEnd,
        MsgType::FedHello,
        MsgType::FedEvent,
        MsgType::FedCue,
        MsgType::ProbeSet,
        MsgType::ProbeData,
    ];
    for (p, t) in sample_payloads().iter().zip(expected) {
        assert_eq!(p.msg_type(), t);
    }
}

// ---- TICK ------------------------------------------------------------------

#[test]
fn every_wall_quality_maps_both_ways_and_orders_by_trust() {
    for (v, q) in [
        (0, WallQuality::None),
        (1, WallQuality::AppSupplied),
        (2, WallQuality::Ntp),
        (3, WallQuality::GpsOrRtc),
    ] {
        assert_eq!(WallQuality::from_u8(v), Some(q));
        assert_eq!(q.to_u8(), v);
    }
    assert_eq!(WallQuality::from_u8(4), None);
    // Ordering is meaningful: a schedule can require "at least NTP".
    assert!(WallQuality::None < WallQuality::Ntp);
    assert!(WallQuality::Ntp < WallQuality::GpsOrRtc);
}

#[test]
fn a_tick_with_an_undefined_wall_quality_is_rejected() {
    // Not treated as a reserved field: an unknown quality would be silently
    // trusted as some other quality, and schedules must degrade explicitly
    // rather than fire at a plausible-looking wrong moment.
    let mut body = [0u8; 64];
    let n = {
        let mut w = Writer::new(&mut body);
        w.u64(1).unwrap();
        w.uuid(&uuid(1)).unwrap();
        w.u32(0).unwrap();
        w.u32(0).unwrap();
        w.u64(0).unwrap();
        w.u8(9).unwrap();
        w.zeros(3).unwrap();
        w.position()
    };
    assert_eq!(
        Payload::decode(MsgType::Tick, &body[..n]),
        Err(DecodeError::InvalidValue {
            field: "TICK.wall_quality"
        })
    );
}

// ---- FRAME -----------------------------------------------------------------

#[test]
fn every_pixel_format_maps_both_ways_with_its_stride() {
    for (v, f, stride) in [
        (0, PixelFormat::Rgb8, 3),
        (1, PixelFormat::Rgbw8, 4),
        (2, PixelFormat::Rgb16, 6),
        (3, PixelFormat::Cct, 4),
    ] {
        assert_eq!(PixelFormat::from_u8(v), Some(f));
        assert_eq!(f.to_u8(), v);
        assert_eq!(f.bytes_per_pixel(), stride);
    }
    assert_eq!(PixelFormat::from_u8(4), None);
}

#[test]
fn frame_round_trips_for_every_format() {
    let pixels = [0x5Au8; 64];
    for f in [
        PixelFormat::Rgb8,
        PixelFormat::Rgbw8,
        PixelFormat::Rgb16,
        PixelFormat::Cct,
    ] {
        let count = 5u16;
        assert_round_trips(&Payload::Frame(Frame {
            segment_id: 1,
            offset: 10,
            format: f,
            priority: 100,
            count,
            pixels: &pixels[..count as usize * f.bytes_per_pixel()],
        }));
    }
}

#[test]
fn a_frame_with_an_unknown_format_is_rejected() {
    let mut body = [0u8; 16];
    let n = {
        let mut w = Writer::new(&mut body);
        w.u16(1).unwrap();
        w.u16(0).unwrap();
        w.u8(99).unwrap();
        w.u8(0).unwrap();
        w.u16(0).unwrap();
        w.position()
    };
    assert_eq!(
        Payload::decode(MsgType::Frame, &body[..n]),
        Err(DecodeError::InvalidValue {
            field: "FRAME.format"
        })
    );
}

#[test]
fn a_frame_whose_count_disagrees_with_its_pixels_is_refused_both_ways() {
    // Encoding: the struct is internally inconsistent, so it must not reach the
    // wire at all.
    let mut buf = [0u8; 64];
    let mut w = Writer::new(&mut buf);
    let bad = Frame {
        segment_id: 0,
        offset: 0,
        format: PixelFormat::Rgb8,
        priority: 0,
        count: 4, // needs 12 bytes
        pixels: &[1, 2, 3],
    };
    assert_eq!(
        bad.encode(&mut w),
        Err(EncodeError::Invalid(DecodeError::InvalidValue {
            field: "FRAME.pixels"
        }))
    );

    // Decoding: the wire promises more pixels than the datagram carries.
    let mut body = [0u8; 16];
    let n = {
        let mut w2 = Writer::new(&mut body);
        w2.u16(0).unwrap();
        w2.u16(0).unwrap();
        w2.u8(0).unwrap();
        w2.u8(0).unwrap();
        w2.u16(100).unwrap();
        w2.position()
    };
    assert_eq!(
        Payload::decode(MsgType::Frame, &body[..n]),
        Err(DecodeError::Truncated)
    );
}

// ---- SRC_PUSH: the "stuck red at 3am" rule ---------------------------------

#[test]
fn a_source_at_the_ambient_floor_may_omit_its_expiry() {
    assert_round_trips(&Payload::SrcPush(SrcPush {
        source_id: uuid(1),
        zone_id: uuid(2),
        scene_id: uuid(3),
        priority: AMBIENT_FLOOR_PRIORITY,
        fade_in_ms: 500,
        fade_out_ms: 500,
        expires_at: None,
        param_overrides: &[],
    }));
}

#[test]
fn a_source_above_the_floor_with_no_expiry_is_refused_on_encode() {
    // Enforced on encode so a buggy client cannot even construct the datagram.
    let bad = SrcPush {
        source_id: uuid(1),
        zone_id: uuid(2),
        scene_id: uuid(3),
        priority: 200,
        fade_in_ms: 0,
        fade_out_ms: 0,
        expires_at: None,
        param_overrides: &[],
    };
    let mut buf = [0u8; 256];
    let mut w = Writer::new(&mut buf);
    assert_eq!(
        bad.encode(&mut w),
        Err(EncodeError::Invalid(DecodeError::SourceWithoutExpiry {
            priority: 200
        }))
    );
}

#[test]
fn a_source_above_the_floor_with_no_expiry_is_refused_on_decode() {
    // And on decode, so a hostile client cannot get one past a conforming
    // device either. This is the case a naive implementation accepts.
    let mut body = [0u8; 256];
    let n = {
        let mut w = Writer::new(&mut body);
        w.uuid(&uuid(1)).unwrap();
        w.uuid(&uuid(2)).unwrap();
        w.uuid(&uuid(3)).unwrap();
        w.u8(200).unwrap(); // priority
        w.u8(0).unwrap(); // flags: no expiry
        w.u16(0).unwrap();
        w.u16(0).unwrap();
        w.zeros(2).unwrap();
        w.blob(&[]).unwrap();
        w.position()
    };
    assert_eq!(
        Payload::decode(MsgType::SrcPush, &body[..n]),
        Err(DecodeError::SourceWithoutExpiry { priority: 200 })
    );
}

#[test]
fn sixty_four_is_the_first_priority_that_must_expire() {
    // The boundary, not just a comfortably-high number. The ambient band is
    // 0-63 and is the floor; 64 is the first thing above it.
    let bad = SrcPush {
        source_id: uuid(1),
        zone_id: uuid(2),
        scene_id: uuid(3),
        priority: 64,
        fade_in_ms: 0,
        fade_out_ms: 0,
        expires_at: None,
        param_overrides: &[],
    };
    let mut buf = [0u8; 256];
    let mut w = Writer::new(&mut buf);
    assert_eq!(
        bad.encode(&mut w),
        Err(EncodeError::Invalid(DecodeError::SourceWithoutExpiry {
            priority: 64
        }))
    );
}

#[test]
fn an_ambient_scene_may_be_immortal_anywhere_in_its_band() {
    // The reason the threshold is 63 rather than 0. An ambient scene at priority
    // 40 with no expiry is not a bug - it is the floor, and a floor on a timer
    // is not a floor.
    for priority in [0u8, 1, 40, AMBIENT_FLOOR_PRIORITY] {
        assert_round_trips(&Payload::SrcPush(SrcPush {
            source_id: uuid(1),
            zone_id: uuid(2),
            scene_id: uuid(3),
            priority,
            fade_in_ms: 0,
            fade_out_ms: 0,
            expires_at: None,
            param_overrides: &[],
        }));
    }
}

// ---- Replicated state ------------------------------------------------------

#[test]
fn state_digest_round_trips_and_iterates() {
    let entries = [
        DigestEntry {
            record_id: uuid(1),
            hlc: 10,
        },
        DigestEntry {
            record_id: uuid(2),
            hlc: 20,
        },
    ];
    let mut buf = [0u8; 256];
    let n = {
        let mut w = Writer::new(&mut buf);
        StateDigest::encode_from(&entries, &mut w).unwrap();
        w.position()
    };
    let d = match Payload::decode(MsgType::StateDigest, &buf[..n]).unwrap() {
        Payload::StateDigest(d) => d,
        other => panic!("wrong variant: {other:?}"),
    };
    assert_eq!(d.count, 2);
    assert_eq!(d.entries().count(), 2);
    let mut it = d.entries();
    assert_eq!(it.next().unwrap(), entries[0]);
    assert_eq!(it.next().unwrap(), entries[1]);
    assert_eq!(it.next(), None);

    let mut out = [0u8; 256];
    let m = {
        let mut w = Writer::new(&mut out);
        d.encode(&mut w).unwrap();
        w.position()
    };
    assert_eq!(&out[..m], &buf[..n]);
}

#[test]
fn an_empty_digest_is_legal() {
    let mut buf = [0u8; 8];
    let n = {
        let mut w = Writer::new(&mut buf);
        StateDigest::encode_from(&[], &mut w).unwrap();
        w.position()
    };
    let d = match Payload::decode(MsgType::StateDigest, &buf[..n]).unwrap() {
        Payload::StateDigest(d) => d,
        other => panic!("wrong variant: {other:?}"),
    };
    assert_eq!(d.count, 0);
    assert_eq!(d.entries().count(), 0);
}

#[test]
fn a_digest_promising_more_entries_than_it_carries_is_truncation() {
    let body = [0x05, 0x00];
    assert_eq!(
        Payload::decode(MsgType::StateDigest, &body),
        Err(DecodeError::Truncated)
    );
}

#[test]
fn state_pull_round_trips_and_iterates() {
    let ids = [uuid(1), uuid(2), uuid(3)];
    let mut buf = [0u8; 256];
    let n = {
        let mut w = Writer::new(&mut buf);
        StatePull::encode_from(&ids, &mut w).unwrap();
        w.position()
    };
    let p = match Payload::decode(MsgType::StatePull, &buf[..n]).unwrap() {
        Payload::StatePull(p) => p,
        other => panic!("wrong variant: {other:?}"),
    };
    assert_eq!(p.count, 3);
    assert!(p.ids().eq(ids));

    let mut out = [0u8; 256];
    let m = {
        let mut w = Writer::new(&mut out);
        p.encode(&mut w).unwrap();
        w.position()
    };
    assert_eq!(&out[..m], &buf[..n]);
}

#[test]
fn state_push_round_trips_variable_length_records() {
    let sig_a = [1u8; 64];
    let sig_b = [2u8; 64];
    let records = [
        StateRecord {
            record_id: uuid(1),
            record_type: 3,
            hlc: 100,
            author: uuid(9),
            body: &[1, 2, 3],
            sig: &sig_a,
        },
        StateRecord {
            record_id: uuid(2),
            record_type: 4,
            hlc: 200,
            author: uuid(8),
            body: &[],
            sig: &sig_b,
        },
    ];
    let mut buf = [0u8; 512];
    let n = {
        let mut w = Writer::new(&mut buf);
        StatePush::encode_from(&records, &mut w).unwrap();
        w.position()
    };
    let p = match Payload::decode(MsgType::StatePush, &buf[..n]).unwrap() {
        Payload::StatePush(p) => p,
        other => panic!("wrong variant: {other:?}"),
    };
    assert_eq!(p.count, 2);
    assert_eq!(p.records().count(), 2);
    let mut it = p.records();
    assert_eq!(it.next().unwrap(), records[0]);
    assert_eq!(it.next().unwrap(), records[1]);

    let mut out = [0u8; 512];
    let m = {
        let mut w = Writer::new(&mut out);
        p.encode(&mut w).unwrap();
        w.position()
    };
    assert_eq!(&out[..m], &buf[..n]);
}

#[test]
fn a_state_push_whose_records_run_off_the_end_is_truncation() {
    let mut buf = [0u8; 64];
    let n = {
        let mut w = Writer::new(&mut buf);
        w.u16(1).unwrap();
        w.uuid(&uuid(1)).unwrap();
        w.u8(0).unwrap();
        w.u64(0).unwrap();
        w.uuid(&uuid(2)).unwrap();
        w.u16(50).unwrap(); // promises 50 body bytes that are not there
        w.position()
    };
    assert_eq!(
        Payload::decode(MsgType::StatePush, &buf[..n]),
        Err(DecodeError::Truncated)
    );
}

#[test]
fn the_signed_byte_range_is_defined_in_exactly_one_place() {
    let sig = [0u8; 64];
    let rec = StateRecord {
        record_id: uuid(1),
        record_type: 7,
        hlc: 0x1122_3344,
        author: uuid(2),
        body: &[9, 9],
        sig: &sig,
    };
    let mut out = [0u8; 128];
    let n = rec.signed_bytes_into(&mut out).unwrap();
    assert_eq!(n, rec.signed_len());
    assert_eq!(n, 16 + 1 + 8 + 16 + 2);

    // record_id ‖ record_type ‖ hlc ‖ author ‖ body, in that order.
    assert_eq!(&out[0..16], uuid(1).as_bytes());
    assert_eq!(out[16], 7);
    assert_eq!(&out[17..25], &0x1122_3344u64.to_le_bytes());
    assert_eq!(&out[25..41], uuid(2).as_bytes());
    assert_eq!(&out[41..43], &[9, 9]);

    // A buffer too small must fail rather than write a partial preimage — a
    // truncated preimage would verify against the wrong bytes.
    let mut tiny = [0u8; 8];
    assert!(rec.signed_bytes_into(&mut tiny).is_err());
}

// ---- TIMECTL ---------------------------------------------------------------

#[test]
fn every_time_mode_maps_both_ways_and_round_trips() {
    for (v, m) in [
        (0, TimeMode::Run),
        (1, TimeMode::Pause),
        (2, TimeMode::Step),
        (3, TimeMode::Set),
    ] {
        assert_eq!(TimeMode::from_u8(v), Some(m));
        assert_eq!(m.to_u8(), v);
        assert_round_trips(&Payload::TimeCtl(TimeCtl {
            mode: m,
            lease_ms: 10_000,
            target_show_time: 42,
        }));
    }
    assert_eq!(TimeMode::from_u8(4), None);
}

#[test]
fn a_timectl_with_an_undefined_mode_is_rejected() {
    let mut body = [0u8; 16];
    let n = {
        let mut w = Writer::new(&mut body);
        w.u8(200).unwrap();
        w.zeros(3).unwrap();
        w.u32(0).unwrap();
        w.u64(0).unwrap();
        w.position()
    };
    assert_eq!(
        Payload::decode(MsgType::TimeCtl, &body[..n]),
        Err(DecodeError::InvalidValue {
            field: "TIMECTL.mode"
        })
    );
}

// ---- Forward compatibility -------------------------------------------------

#[test]
fn messages_the_spec_has_not_laid_out_stay_raw() {
    // HELLO, CAPS, GET and SET are named in the type table but have no byte
    // layout. Guessing one here would put an invention on the wire under a
    // normative name.
    for t in [MsgType::Hello, MsgType::Caps, MsgType::Get, MsgType::Set] {
        let body = [1u8, 2, 3];
        let p = Payload::decode(t, &body).unwrap();
        assert_eq!(
            p,
            Payload::Unparsed {
                msg_type: t,
                body: &body
            }
        );
        assert_eq!(p.msg_type(), t);

        let mut out = [0u8; 8];
        let n = {
            let mut w = Writer::new(&mut out);
            p.encode(&mut w).unwrap();
            w.position()
        };
        assert_eq!(&out[..n], &body);
    }
}

#[test]
fn trailing_bytes_after_a_payload_are_tolerated() {
    // A peer one minor version ahead may have appended a field. Refusing that
    // would defeat forward compatibility, which is what the version nibble buys.
    let mut body = [0u8; 32];
    let n = {
        let mut w = Writer::new(&mut body);
        w.u64(7).unwrap();
        w.bytes(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
        w.position()
    };
    assert_eq!(
        Payload::decode(MsgType::SyncReq, &body[..n]),
        Ok(Payload::SyncReq(SyncReq { t1: 7 }))
    );
}

#[test]
fn an_empty_body_is_refused_for_every_type_that_needs_one() {
    for t in [
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
    ] {
        assert_eq!(
            Payload::decode(t, &[]),
            Err(DecodeError::Truncated),
            "{t:?} accepted an empty body"
        );
    }
}

// ---- replication ------------------------------------------------------------
//
// The three STATE_* messages are the only ones whose payload is a variable-length
// list the codec has to walk rather than a fixed struct, and they were the only
// three never driven through `Payload`. That matters more than the count
// suggests: replication is what makes a keeper's death survivable, and a codec
// that cannot carry a digest cannot resync a mesh that has split.

#[test]
fn a_state_digest_round_trips_and_yields_its_entries() {
    let entries = [
        DigestEntry {
            record_id: uuid(1),
            hlc: 0x0102_0304_0506_0708,
        },
        DigestEntry {
            record_id: uuid(2),
            hlc: 9,
        },
    ];

    let mut buf = [0u8; 256];
    let n = {
        let mut w = Writer::new(&mut buf);
        StateDigest::encode_from(&entries, &mut w).expect("encode_from");
        w.position()
    };

    let mut r = Reader::new(&buf[..n]);
    let digest = StateDigest::decode(&mut r).expect("decode");
    assert_eq!(digest.count, 2);
    let got: Vec<DigestEntry> = digest.entries().collect();
    assert_eq!(got, entries.to_vec(), "entries must survive the walk");

    let payload = Payload::StateDigest(digest);
    assert_eq!(payload.msg_type(), MsgType::StateDigest);
    assert_round_trips(&payload);
}

#[test]
fn an_empty_state_digest_is_legal() {
    // "I have nothing" is a real answer during gossip, not an error.
    let mut buf = [0u8; 8];
    let n = {
        let mut w = Writer::new(&mut buf);
        StateDigest::encode_from(&[], &mut w).expect("encode_from");
        w.position()
    };
    let mut r = Reader::new(&buf[..n]);
    let digest = StateDigest::decode(&mut r).expect("decode");
    assert_eq!(digest.count, 0);
    assert_eq!(digest.entries().count(), 0);
    assert_round_trips(&Payload::StateDigest(digest));
}

#[test]
fn a_state_pull_round_trips_and_yields_its_ids() {
    let ids = [uuid(7), uuid(8), uuid(9)];
    let mut buf = [0u8; 256];
    let n = {
        let mut w = Writer::new(&mut buf);
        StatePull::encode_from(&ids, &mut w).expect("encode_from");
        w.position()
    };

    let mut r = Reader::new(&buf[..n]);
    let pull = StatePull::decode(&mut r).expect("decode");
    assert_eq!(pull.count, 3);
    assert_eq!(pull.ids().collect::<Vec<Uuid>>(), ids.to_vec());

    let payload = Payload::StatePull(pull);
    assert_eq!(payload.msg_type(), MsgType::StatePull);
    assert_round_trips(&payload);
}

#[test]
fn a_state_push_round_trips_records_of_differing_body_lengths() {
    // Records are variable length and the message carries no byte count, so the
    // decoder has to walk them. Two different body lengths is the smallest case
    // that would catch a decoder assuming a fixed stride.
    let sig_a = [0xAAu8; 64];
    let sig_b = [0xBBu8; 64];
    let records = [
        StateRecord {
            record_id: uuid(1),
            record_type: 3,
            hlc: 100,
            author: uuid(2),
            body: b"first",
            sig: &sig_a,
        },
        StateRecord {
            record_id: uuid(3),
            record_type: 4,
            hlc: 101,
            author: uuid(4),
            body: b"",
            sig: &sig_b,
        },
    ];

    let mut buf = [0u8; 512];
    let n = {
        let mut w = Writer::new(&mut buf);
        StatePush::encode_from(&records, &mut w).expect("encode_from");
        w.position()
    };

    let mut r = Reader::new(&buf[..n]);
    let push = StatePush::decode(&mut r).expect("decode");
    assert_eq!(push.count, 2);
    assert_eq!(
        r.remaining(),
        0,
        "decode must consume exactly the records and no more"
    );

    let got: Vec<StateRecord<'_>> = push.records().collect();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].body, b"first");
    assert_eq!(got[0].sig, &sig_a);
    assert_eq!(got[1].body, b"", "an empty body must survive");
    assert_eq!(got[1].record_type, 4);

    let payload = Payload::StatePush(push);
    assert_eq!(payload.msg_type(), MsgType::StatePush);
    assert_round_trips(&payload);
}

#[test]
fn a_truncated_state_push_is_refused_rather_than_walked_off_the_end() {
    let sig = [0u8; 64];
    let records = [StateRecord {
        record_id: uuid(1),
        record_type: 1,
        hlc: 5,
        author: uuid(2),
        body: b"body",
        sig: &sig,
    }];
    let mut buf = [0u8; 512];
    let n = {
        let mut w = Writer::new(&mut buf);
        StatePush::encode_from(&records, &mut w).expect("encode_from");
        w.position()
    };

    // Every proper prefix must be rejected: the count says one record is
    // coming, and anything short of all of it is malformed.
    for cut in 2..n {
        let mut r = Reader::new(&buf[..cut]);
        assert!(
            StatePush::decode(&mut r).is_err(),
            "a {cut}-byte prefix of a {n}-byte STATE_PUSH must be refused"
        );
    }
}
