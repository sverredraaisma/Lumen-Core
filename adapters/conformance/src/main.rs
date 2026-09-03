//! The conformance adapter for `lumen-proto`.
//!
//! `lumen-proto` is hand-written rather than generated from the IDL, and the
//! stated reason that is safe is that CI round-trips every codec vector against
//! it. This binary is what makes that sentence true; without it the vectors ran
//! only against the reference fixture, which answers from the corpus and so
//! passes by construction.
//!
//! It claims `codec` only. The state machines are `lumen-device`'s, behind the
//! licence boundary, and they have their own adapter.
//!
//! # Both directions
//!
//! `decode` and `encode` are separate paths through separate code. Testing only
//! one is what lets an encoder and a decoder drift together and agree with each
//! other forever, which is the failure the corpus exists to catch.

use std::io::{BufRead, Write};

use lumen_conformance::hex;
use lumen_conformance::json::{parse, Json};
use lumen_proto::buf::{Reader, Writer};
use lumen_proto::header::{Flags, Header, HEADER_LEN, TAG_LEN};
use lumen_proto::msg::*;
use lumen_proto::{DecodeError, MsgType, Payload, Uuid};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { return };
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let response = respond(line);
        if writeln!(stdout, "{response}").is_err() || stdout.flush().is_err() {
            return;
        }
    }
}

fn respond(line: &str) -> String {
    let (verb, rest) = match line.split_once(' ') {
        Some((v, r)) => (v, r.trim()),
        None => (line, "{}"),
    };
    let body = match parse(rest) {
        Ok(j) => j,
        Err(e) => return format!("error {verb} body is not json: {e}"),
    };
    match verb {
        "hello" => r#"ok {"name":"lumen-proto 0.1.0","protocol":2,"kinds":["codec"]}"#.to_string(),
        "decode" => decode(&body),
        "encode" => encode(&body),
        "reset" | "event" => "error this adapter runs codec vectors only".to_string(),
        other => format!("error unknown request verb `{other}`"),
    }
}

// ---- decode ----------------------------------------------------------------

fn decode(body: &Json) -> String {
    let Some(text) = body.get("datagram").and_then(Json::as_str) else {
        return "error decode needs a `datagram`".to_string();
    };
    let bytes = match hex::decode(text) {
        Ok(b) => b,
        Err(e) => return format!("error datagram is not hex: {e}"),
    };

    let dg = match lumen_proto::Datagram::decode(&bytes) {
        Ok(d) => d,
        Err(e) => return format!("reject {e:?}"),
    };

    // An unknown message type is dropped **silently**. Answering `reject` here
    // is the single most consequential conformance failure in the suite: it
    // looks healthy and breaks every future minor version of the protocol.
    let Some(msg_type) = MsgType::from_u8(dg.header.msg_type) else {
        return "ignore".to_string();
    };

    let payload = match Payload::decode(msg_type, dg.payload) {
        Ok(p) => p,
        Err(e) => return format!("reject {e:?}"),
    };
    if let Payload::Unparsed { .. } = payload {
        // A type the spec names but whose payload it has not fixed. Nothing to
        // compare, so treat it as unknown rather than inventing a shape.
        return "ignore".to_string();
    }

    format!(
        r#"ok {{"header":{},"tag":"{}","payload":{}}}"#,
        header_json(&dg.header),
        hex::encode(dg.tag),
        payload_json(&payload)
    )
}

fn header_json(h: &Header) -> String {
    format!(
        concat!(
            r#"{{"magic":{},"version_major":{},"version_minor":{},"type":{},"#,
            r#""flags":{},"mesh_prefix":"{}","sender_prefix":"{}","#,
            r#""sequence":{},"show_time_us":{},"payload_len":{}}}"#
        ),
        lumen_proto::header::MAGIC,
        h.version_major,
        h.version_minor,
        h.msg_type,
        h.flags.0,
        hex::encode(&h.mesh_prefix),
        hex::encode(&h.sender_prefix),
        h.sequence,
        h.show_time_us,
        h.payload_len,
    )
}

fn uuid_hex(u: &Uuid) -> String {
    hex::encode(&u.0)
}

/// JSON string with the few escapes a `str` field can legally contain.
fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn payload_json(p: &Payload<'_>) -> String {
    match p {
        Payload::Tick(m) => format!(
            r#"{{"show_time_us":{},"master_uuid":"{}","master_capacity":{},"election_epoch":{},"wall_time_us":{},"wall_quality":{}}}"#,
            m.show_time_us,
            uuid_hex(&m.master_uuid),
            m.master_capacity,
            m.election_epoch,
            m.wall_time_us,
            m.wall_quality.to_u8()
        ),
        Payload::SyncReq(m) => format!(r#"{{"t1":{}}}"#, m.t1),
        Payload::SyncResp(m) => {
            format!(r#"{{"t1":{},"t2":{},"t3":{}}}"#, m.t1, m.t2, m.t3)
        }
        Payload::Activate(m) => format!(
            r#"{{"program_id":{},"slot":{},"activate_at":{}}}"#,
            m.program_id, m.slot, m.activate_at
        ),
        Payload::Chan(m) => format!(
            r#"{{"channel_id":{},"producer_seq":{},"payload":"{}"}}"#,
            m.channel_id,
            m.producer_seq,
            hex::encode(m.payload)
        ),
        Payload::ChanClaim(m) => format!(
            r#"{{"channel_id":{},"priority":{},"lease_ms":{}}}"#,
            m.channel_id, m.priority, m.lease_ms
        ),
        Payload::ChanRelease(m) => format!(r#"{{"channel_id":{}}}"#, m.channel_id),
        Payload::Frame(m) => format!(
            r#"{{"segment_id":{},"offset":{},"format":{},"priority":{},"count":{},"pixels":"{}"}}"#,
            m.segment_id,
            m.offset,
            m.format.to_u8(),
            m.priority,
            m.count,
            hex::encode(m.pixels)
        ),
        Payload::SrcPush(m) => format!(
            r#"{{"source_id":"{}","zone_id":"{}","scene_id":"{}","priority":{},"fade_in_ms":{},"fade_out_ms":{},"expires_at":{},"param_overrides":"{}"}}"#,
            uuid_hex(&m.source_id),
            uuid_hex(&m.zone_id),
            uuid_hex(&m.scene_id),
            m.priority,
            m.fade_in_ms,
            m.fade_out_ms,
            // `null`, not zero: the flag bit is clear and the field is absent
            // from the wire entirely.
            match m.expires_at {
                Some(v) => v.to_string(),
                None => "null".to_string(),
            },
            hex::encode(m.param_overrides)
        ),
        Payload::SrcRenew(m) => format!(
            r#"{{"source_id":"{}","expires_at":{}}}"#,
            uuid_hex(&m.source_id),
            m.expires_at
        ),
        Payload::SrcPop(m) => format!(
            r#"{{"source_id":"{}","fade_out_ms":{}}}"#,
            uuid_hex(&m.source_id),
            m.fade_out_ms
        ),
        Payload::Event(m) => format!(
            r#"{{"event_id":"{}","source_uuid":"{}","kind":{},"value":{},"wall_time_us":{}}}"#,
            uuid_hex(&m.event_id),
            uuid_hex(&m.source_uuid),
            json_str(m.kind),
            // q16 is the raw i32, not the divided value.
            m.value,
            m.wall_time_us
        ),
        Payload::StateDigest(m) => {
            let entries: Vec<String> = m
                .entries()
                .map(|e| {
                    format!(
                        r#"{{"record_id":"{}","hlc":{}}}"#,
                        uuid_hex(&e.record_id),
                        e.hlc
                    )
                })
                .collect();
            format!(r#"{{"entries":[{}]}}"#, entries.join(","))
        }
        Payload::StatePull(m) => {
            let ids: Vec<String> = m.ids().map(|i| format!("\"{}\"", uuid_hex(&i))).collect();
            format!(r#"{{"ids":[{}]}}"#, ids.join(","))
        }
        Payload::StatePush(m) => {
            let records: Vec<String> = m
                .records()
                .map(|r| {
                    format!(
                        r#"{{"record_id":"{}","record_type":{},"hlc":{},"author":"{}","body":"{}","sig":"{}"}}"#,
                        uuid_hex(&r.record_id),
                        r.record_type,
                        r.hlc,
                        uuid_hex(&r.author),
                        hex::encode(r.body),
                        hex::encode(r.sig)
                    )
                })
                .collect();
            format!(r#"{{"records":[{}]}}"#, records.join(","))
        }
        Payload::ProgBegin(m) => format!(
            r#"{{"program_id":{},"slot":{},"vm_min_version":{},"total_len":{},"device_class":{}}}"#,
            m.program_id,
            m.slot,
            m.vm_min_version,
            m.total_len,
            json_str(m.device_class)
        ),
        Payload::ProgChunk(m) => format!(
            r#"{{"program_id":{},"offset":{},"data":"{}"}}"#,
            m.program_id,
            m.offset,
            hex::encode(m.data)
        ),
        Payload::ProgEnd(m) => format!(
            r#"{{"program_id":{},"sha256":"{}","sig":"{}"}}"#,
            m.program_id,
            hex::encode(&m.sha256),
            hex::encode(&m.sig)
        ),
        Payload::FedHello(m) => format!(
            r#"{{"mesh_id":"{}","mesh_name":{},"caps":{},"fed_pubkey":"{}"}}"#,
            uuid_hex(&m.mesh_id),
            json_str(m.mesh_name),
            m.caps,
            hex::encode(&m.fed_pubkey)
        ),
        Payload::FedEvent(m) => format!(
            r#"{{"event":{},"origin_mesh":"{}"}}"#,
            payload_json(&Payload::Event(m.event)),
            uuid_hex(&m.origin_mesh)
        ),
        Payload::FedCue(m) => format!(
            r#"{{"cue_name":{},"wall_at_us":{},"origin_mesh":"{}"}}"#,
            json_str(m.cue_name),
            m.wall_at_us,
            uuid_hex(&m.origin_mesh)
        ),
        Payload::ProbeSet(m) => format!(
            r#"{{"program_id":{},"probe_id":{},"pixel_index":{}}}"#,
            m.program_id, m.probe_id, m.pixel_index
        ),
        Payload::ProbeData(m) => format!(
            r#"{{"probe_id":{},"pixel_index":{},"frame_show_time":{},"value":{}}}"#,
            m.probe_id, m.pixel_index, m.frame_show_time, m.value
        ),
        Payload::TimeCtl(m) => format!(
            r#"{{"mode":{},"lease_ms":{},"target_show_time":{}}}"#,
            m.mode.to_u8(),
            m.lease_ms,
            m.target_show_time
        ),
        Payload::Unparsed { .. } => "null".to_string(),
    }
}

// ---- encode ----------------------------------------------------------------

fn encode(body: &Json) -> String {
    match build(body) {
        Ok(bytes) => format!(r#"ok {{"datagram":"{}"}}"#, hex::encode(&bytes)),
        Err(e) => format!("error {e}"),
    }
}

fn build(body: &Json) -> Result<Vec<u8>, String> {
    let h = body.get("header").ok_or("value has no `header`")?;
    let tag_hex = body
        .get("tag")
        .and_then(Json::as_str)
        .ok_or("value has no `tag`")?;
    let tag = hex::decode(tag_hex).map_err(|e| format!("tag: {e}"))?;
    if tag.len() != TAG_LEN {
        return Err(format!("tag must be {TAG_LEN} bytes, got {}", tag.len()));
    }

    let msg_type_num = u64_of(h, "type")? as u8;
    let msg_type = MsgType::from_u8(msg_type_num)
        .ok_or_else(|| format!("type 0x{msg_type_num:02x} is not a known message"))?;

    // The payload first: `payload_len` is a function of it, and the one field a
    // hand-written encoder gets wrong is the one it has to derive.
    let payload = body.get("payload").ok_or("value has no `payload`")?;
    let mut body_buf = vec![0u8; 65535];
    let written = write_payload(msg_type, payload, &mut body_buf)?;
    body_buf.truncate(written);

    let mut header = Header::new(
        msg_type,
        prefix2(h, "mesh_prefix")?,
        prefix4(h, "sender_prefix")?,
        u64_of(h, "sequence")? as u32,
        u64_of(h, "show_time_us")?,
    );
    header.version_major = u64_of(h, "version_major")? as u8;
    header.version_minor = u64_of(h, "version_minor")? as u8;
    header.flags = Flags(u64_of(h, "flags")? as u8);
    header.payload_len = written as u16;

    let mut out = vec![0u8; HEADER_LEN + written + TAG_LEN];
    header
        .encode(&mut out[..HEADER_LEN])
        .map_err(|e| format!("header: {e:?}"))?;
    out[HEADER_LEN..HEADER_LEN + written].copy_from_slice(&body_buf);
    out[HEADER_LEN + written..].copy_from_slice(&tag);
    Ok(out)
}

fn u64_of(j: &Json, key: &str) -> Result<u64, String> {
    j.get(key)
        .and_then(Json::as_u64)
        .ok_or_else(|| format!("`{key}` is missing or not a number"))
}

/// A `q16` arrives as its raw `i32`, which may be negative — so it cannot go
/// through `as_u64`, which stops at zero.
fn q16_of(j: &Json, key: &str) -> Result<i32, String> {
    match j.get(key) {
        Some(Json::Number(text)) => text
            .parse::<i64>()
            .map(|v| v as i32)
            .map_err(|_| format!("`{key}` is not an integer")),
        _ => Err(format!("`{key}` is missing or not a number")),
    }
}

fn hex_of(j: &Json, key: &str) -> Result<Vec<u8>, String> {
    let text = j
        .get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| format!("`{key}` is missing or not a string"))?;
    hex::decode(text).map_err(|e| format!("{key}: {e}"))
}

fn str_of<'a>(j: &'a Json, key: &str) -> Result<&'a str, String> {
    j.get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| format!("`{key}` is missing or not a string"))
}

fn uuid_of(j: &Json, key: &str) -> Result<Uuid, String> {
    let b = hex_of(j, key)?;
    let a: [u8; 16] = b
        .as_slice()
        .try_into()
        .map_err(|_| format!("`{key}` must be 16 bytes"))?;
    Ok(Uuid(a))
}

fn prefix2(j: &Json, key: &str) -> Result<[u8; 2], String> {
    let b = hex_of(j, key)?;
    b.as_slice()
        .try_into()
        .map_err(|_| format!("`{key}` must be 2 bytes"))
}

fn prefix4(j: &Json, key: &str) -> Result<[u8; 4], String> {
    let b = hex_of(j, key)?;
    b.as_slice()
        .try_into()
        .map_err(|_| format!("`{key}` must be 4 bytes"))
}

fn fixed<const N: usize>(j: &Json, key: &str) -> Result<[u8; N], String> {
    let b = hex_of(j, key)?;
    b.as_slice()
        .try_into()
        .map_err(|_| format!("`{key}` must be {N} bytes"))
}

fn write_payload(msg_type: MsgType, p: &Json, out: &mut [u8]) -> Result<usize, String> {
    let mut w = Writer::new(out);
    let e = |e: lumen_proto::EncodeError| format!("{e:?}");

    match msg_type {
        MsgType::Tick => Tick {
            show_time_us: u64_of(p, "show_time_us")?,
            master_uuid: uuid_of(p, "master_uuid")?,
            master_capacity: u64_of(p, "master_capacity")? as u32,
            election_epoch: u64_of(p, "election_epoch")? as u32,
            wall_time_us: u64_of(p, "wall_time_us")?,
            wall_quality: WallQuality::from_u8(u64_of(p, "wall_quality")? as u8)
                .ok_or("wall_quality is not a known value")?,
        }
        .encode(&mut w)
        .map_err(e)?,
        MsgType::SyncReq => SyncReq {
            t1: u64_of(p, "t1")?,
        }
        .encode(&mut w)
        .map_err(e)?,
        MsgType::SyncResp => SyncResp {
            t1: u64_of(p, "t1")?,
            t2: u64_of(p, "t2")?,
            t3: u64_of(p, "t3")?,
        }
        .encode(&mut w)
        .map_err(e)?,
        MsgType::Activate => Activate {
            program_id: u64_of(p, "program_id")? as u16,
            slot: u64_of(p, "slot")? as u8,
            activate_at: u64_of(p, "activate_at")?,
        }
        .encode(&mut w)
        .map_err(e)?,
        MsgType::Chan => {
            let payload = hex_of(p, "payload")?;
            Chan {
                channel_id: u64_of(p, "channel_id")? as u16,
                producer_seq: u64_of(p, "producer_seq")? as u16,
                payload: &payload,
            }
            .encode(&mut w)
            .map_err(e)?
        }
        MsgType::ChanClaim => ChanClaim {
            channel_id: u64_of(p, "channel_id")? as u16,
            priority: u64_of(p, "priority")? as u8,
            lease_ms: u64_of(p, "lease_ms")? as u32,
        }
        .encode(&mut w)
        .map_err(e)?,
        MsgType::ChanRelease => ChanRelease {
            channel_id: u64_of(p, "channel_id")? as u16,
        }
        .encode(&mut w)
        .map_err(e)?,
        MsgType::Frame => {
            let pixels = hex_of(p, "pixels")?;
            Frame {
                segment_id: u64_of(p, "segment_id")? as u16,
                offset: u64_of(p, "offset")? as u16,
                format: PixelFormat::from_u8(u64_of(p, "format")? as u8)
                    .ok_or("format is not a known value")?,
                priority: u64_of(p, "priority")? as u8,
                count: u64_of(p, "count")? as u16,
                pixels: &pixels,
            }
            .encode(&mut w)
            .map_err(e)?
        }
        MsgType::SrcPush => {
            let overrides = hex_of(p, "param_overrides")?;
            SrcPush {
                source_id: uuid_of(p, "source_id")?,
                zone_id: uuid_of(p, "zone_id")?,
                scene_id: uuid_of(p, "scene_id")?,
                priority: u64_of(p, "priority")? as u8,
                fade_in_ms: u64_of(p, "fade_in_ms")? as u16,
                fade_out_ms: u64_of(p, "fade_out_ms")? as u16,
                // `null` means the flag bit is clear and the field is absent,
                // which is not the same as zero.
                expires_at: match p.get("expires_at") {
                    Some(Json::Null) | None => None,
                    Some(_) => Some(u64_of(p, "expires_at")?),
                },
                param_overrides: &overrides,
            }
            .encode(&mut w)
            .map_err(e)?
        }
        MsgType::SrcRenew => SrcRenew {
            source_id: uuid_of(p, "source_id")?,
            expires_at: u64_of(p, "expires_at")?,
        }
        .encode(&mut w)
        .map_err(e)?,
        MsgType::SrcPop => SrcPop {
            source_id: uuid_of(p, "source_id")?,
            fade_out_ms: u64_of(p, "fade_out_ms")? as u16,
        }
        .encode(&mut w)
        .map_err(e)?,
        MsgType::Event => {
            let kind = str_of(p, "kind")?;
            event_of(p, kind).encode(&mut w).map_err(e)?
        }
        MsgType::StateDigest => {
            let entries = digest_entries(p)?;
            StateDigest::encode_from(&entries, &mut w).map_err(e)?
        }
        MsgType::StatePull => {
            let ids = pull_ids(p)?;
            StatePull::encode_from(&ids, &mut w).map_err(e)?
        }
        MsgType::StatePush => {
            // The records borrow their bodies, so the owned buffers have to
            // outlive the slice handed to `encode_from`.
            let owned = push_records(p)?;
            let records: Vec<StateRecord<'_>> = owned
                .iter()
                .map(|(id, ty, hlc, author, body, sig)| StateRecord {
                    record_id: *id,
                    record_type: *ty,
                    hlc: *hlc,
                    author: *author,
                    body,
                    sig,
                })
                .collect();
            StatePush::encode_from(&records, &mut w).map_err(e)?
        }
        MsgType::ProgBegin => {
            let class = str_of(p, "device_class")?;
            ProgBegin {
                program_id: u64_of(p, "program_id")? as u16,
                slot: u64_of(p, "slot")? as u8,
                vm_min_version: u64_of(p, "vm_min_version")? as u8,
                total_len: u64_of(p, "total_len")? as u32,
                device_class: class,
            }
            .encode(&mut w)
            .map_err(e)?
        }
        MsgType::ProgChunk => {
            let data = hex_of(p, "data")?;
            ProgChunk {
                program_id: u64_of(p, "program_id")? as u16,
                offset: u64_of(p, "offset")? as u32,
                data: &data,
            }
            .encode(&mut w)
            .map_err(e)?
        }
        MsgType::ProgEnd => ProgEnd {
            program_id: u64_of(p, "program_id")? as u16,
            sha256: fixed::<32>(p, "sha256")?,
            sig: fixed::<64>(p, "sig")?,
        }
        .encode(&mut w)
        .map_err(e)?,
        MsgType::FedHello => {
            let name = str_of(p, "mesh_name")?;
            FedHello {
                mesh_id: uuid_of(p, "mesh_id")?,
                mesh_name: name,
                caps: u64_of(p, "caps")? as u32,
                fed_pubkey: fixed::<32>(p, "fed_pubkey")?,
            }
            .encode(&mut w)
            .map_err(e)?
        }
        MsgType::FedEvent => {
            let inner = p.get("event").ok_or("FED_EVENT has no `event`")?;
            let kind = str_of(inner, "kind")?;
            FedEvent {
                event: event_of(inner, kind),
                origin_mesh: uuid_of(p, "origin_mesh")?,
            }
            .encode(&mut w)
            .map_err(e)?
        }
        MsgType::FedCue => {
            let name = str_of(p, "cue_name")?;
            FedCue {
                cue_name: name,
                wall_at_us: u64_of(p, "wall_at_us")?,
                origin_mesh: uuid_of(p, "origin_mesh")?,
            }
            .encode(&mut w)
            .map_err(e)?
        }
        MsgType::ProbeSet => ProbeSet {
            program_id: u64_of(p, "program_id")? as u16,
            probe_id: u64_of(p, "probe_id")? as u16,
            pixel_index: u64_of(p, "pixel_index")? as u16,
        }
        .encode(&mut w)
        .map_err(e)?,
        MsgType::ProbeData => ProbeData {
            probe_id: u64_of(p, "probe_id")? as u16,
            pixel_index: u64_of(p, "pixel_index")? as u16,
            frame_show_time: u64_of(p, "frame_show_time")?,
            value: q16_of(p, "value")?,
        }
        .encode(&mut w)
        .map_err(e)?,
        MsgType::TimeCtl => TimeCtl {
            mode: TimeMode::from_u8(u64_of(p, "mode")? as u8).ok_or("mode is not a known value")?,
            lease_ms: u64_of(p, "lease_ms")? as u32,
            target_show_time: u64_of(p, "target_show_time")?,
        }
        .encode(&mut w)
        .map_err(e)?,
        other => return Err(format!("{other:?} has no payload layout yet")),
    }
    Ok(w.position())
}

fn event_of<'a>(p: &'a Json, kind: &'a str) -> Event<'a> {
    Event {
        event_id: uuid_of(p, "event_id").unwrap_or(Uuid([0; 16])),
        source_uuid: uuid_of(p, "source_uuid").unwrap_or(Uuid([0; 16])),
        kind,
        value: q16_of(p, "value").unwrap_or(0),
        wall_time_us: u64_of(p, "wall_time_us").unwrap_or(0),
    }
}

fn digest_entries(p: &Json) -> Result<Vec<DigestEntry>, String> {
    let arr = p
        .get("entries")
        .and_then(Json::as_array)
        .ok_or("STATE_DIGEST has no `entries` array")?;
    arr.iter()
        .map(|e| {
            Ok(DigestEntry {
                record_id: uuid_of(e, "record_id")?,
                hlc: u64_of(e, "hlc")?,
            })
        })
        .collect()
}

fn pull_ids(p: &Json) -> Result<Vec<Uuid>, String> {
    let arr = p
        .get("ids")
        .and_then(Json::as_array)
        .ok_or("STATE_PULL has no `ids` array")?;
    arr.iter()
        .map(|j| {
            let text = j.as_str().ok_or("an id must be a hex string")?;
            let b = hex::decode(text).map_err(|e| format!("id: {e}"))?;
            let a: [u8; 16] = b
                .as_slice()
                .try_into()
                .map_err(|_| "an id must be 16 bytes")?;
            Ok(Uuid(a))
        })
        .collect()
}

type OwnedRecord = (Uuid, u8, u64, Uuid, Vec<u8>, [u8; 64]);

fn push_records(p: &Json) -> Result<Vec<OwnedRecord>, String> {
    let arr = p
        .get("records")
        .and_then(Json::as_array)
        .ok_or("STATE_PUSH has no `records` array")?;
    arr.iter()
        .map(|r| {
            Ok((
                uuid_of(r, "record_id")?,
                u64_of(r, "record_type")? as u8,
                u64_of(r, "hlc")?,
                uuid_of(r, "author")?,
                hex_of(r, "body")?,
                fixed::<64>(r, "sig")?,
            ))
        })
        .collect()
}

/// Keep the unused-import lint honest about `DecodeError`, which appears only
/// in the `{e:?}` formatting above.
#[allow(dead_code)]
fn _uses(_: DecodeError, _: Reader<'_>) {}
