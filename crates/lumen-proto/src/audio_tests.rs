//! Audio payload codec tests.
//!
//! Both directions, always. An encoder and a decoder that are only ever tested
//! against each other drift together and stay consistent while being wrong,
//! which is the failure the codec vectors exist to prevent everywhere else.

use super::*;

fn sample() -> AudioFrame {
    let mut bands = [0u8; BANDS];
    for (i, b) in bands.iter_mut().enumerate() {
        *b = (i * 8) as u8;
    }
    AudioFrame {
        bands,
        level: 200,
        smoothed_level: 180,
        onset: true,
        beat_phase: 0x8000,
        bpm_x4: 481,
        confidence: 240,
    }
}

fn encoded(f: &AudioFrame) -> [u8; AUDIO_FRAME_LEN] {
    let mut buf = [0u8; AUDIO_FRAME_LEN];
    let mut w = Writer::new(&mut buf);
    f.encode(&mut w).expect("encode");
    assert_eq!(w.position(), AUDIO_FRAME_LEN, "the frame is a fixed size");
    buf
}

#[test]
fn a_frame_round_trips() {
    let f = sample();
    let bytes = encoded(&f);
    let back = AudioFrame::decode(&mut Reader::new(&bytes)).expect("decode");
    assert_eq!(back, f);

    // And re-encoding must reproduce identical bytes.
    assert_eq!(encoded(&back), bytes);
}

#[test]
fn the_layout_is_the_one_that_is_documented() {
    // Byte offsets, asserted directly. This is the half a round trip cannot
    // check: both sides could agree on a different layout entirely.
    let f = sample();
    let b = encoded(&f);
    assert_eq!(&b[..BANDS], &f.bands[..], "0..32 bands");
    assert_eq!(b[32], 200, "32 level");
    assert_eq!(b[33], 180, "33 smoothed");
    assert_eq!(b[34], FLAG_ONSET, "34 flags");
    assert_eq!(b[35], 240, "35 confidence");
    assert_eq!(&b[36..38], &0x8000u16.to_le_bytes(), "36..38 beat phase");
    assert_eq!(&b[38..40], &481u16.to_le_bytes(), "38..40 bpm x4");
    assert_eq!(AUDIO_FRAME_LEN, 40);
}

#[test]
fn a_default_frame_is_all_zeros_on_the_wire() {
    // What an analyser publishes before it has heard anything, and what a
    // device should read as silence rather than as a missing channel.
    assert_eq!(encoded(&AudioFrame::default()), [0u8; AUDIO_FRAME_LEN]);
}

#[test]
fn the_onset_flag_is_the_only_bit_written() {
    let off = AudioFrame {
        onset: false,
        ..Default::default()
    };
    assert_eq!(encoded(&off)[34], 0);
    let on = AudioFrame {
        onset: true,
        ..Default::default()
    };
    assert_eq!(encoded(&on)[34], 1);
}

#[test]
fn unknown_flag_bits_are_ignored_rather_than_refused() {
    // The rule that makes a minor-version addition safe. A device that refused
    // a frame carrying a bit it did not know would go dark on an upgrade
    // somewhere else in the mesh.
    let mut bytes = encoded(&AudioFrame::default());
    bytes[34] = 0b1111_1110;
    let f = AudioFrame::decode(&mut Reader::new(&bytes)).expect("decode");
    assert!(!f.onset, "none of those bits is the onset flag");

    bytes[34] = 0b1111_1111;
    let f = AudioFrame::decode(&mut Reader::new(&bytes)).expect("decode");
    assert!(f.onset);
}

#[test]
fn every_byte_pattern_decodes() {
    // There is no invalid frame: every field is a plain integer over its whole
    // range. That is deliberate — a malformed audio frame would be one more
    // failure mode on the hottest path in the system.
    for fill in [0x00u8, 0x01, 0x7F, 0x80, 0xFF, 0xAA, 0x55] {
        let bytes = [fill; AUDIO_FRAME_LEN];
        AudioFrame::decode(&mut Reader::new(&bytes)).expect("every pattern is a frame");
    }
}

#[test]
fn a_short_payload_is_refused() {
    let bytes = encoded(&sample());
    for cut in 0..AUDIO_FRAME_LEN {
        assert!(
            AudioFrame::from_payload(&bytes[..cut]).is_err(),
            "{cut} bytes must not decode"
        );
    }
    assert!(AudioFrame::from_payload(&bytes).is_ok());
}

#[test]
fn a_longer_payload_is_accepted_and_the_extra_ignored() {
    // Forward compatibility in the other direction: a later analyser appending
    // a field must not take the mesh's audio away from an older device.
    let mut long = [0u8; AUDIO_FRAME_LEN + 8];
    long[..AUDIO_FRAME_LEN].copy_from_slice(&encoded(&sample()));
    long[AUDIO_FRAME_LEN..].fill(0xEE);
    assert_eq!(AudioFrame::from_payload(&long).expect("decode"), sample());
}

#[test]
fn the_tempo_reads_back_in_whole_bpm() {
    for (x4, bpm) in [(0u16, 0u16), (4, 1), (481, 120), (600, 150), (799, 199)] {
        let f = AudioFrame {
            bpm_x4: x4,
            ..Default::default()
        };
        assert_eq!(f.bpm(), bpm, "{x4} quarter-BPM");
    }
}

#[test]
fn a_band_past_the_end_reads_as_silence() {
    // The index comes from a compiled program, so it is not trustworthy, and a
    // device must not die because an effect asked for band 40.
    let f = sample();
    assert_eq!(f.band(0), 0);
    assert_eq!(f.band(31), 248);
    assert_eq!(f.band(32), 0, "past the end");
    assert_eq!(f.band(usize::MAX), 0);
}

#[test]
fn encoding_into_too_small_a_buffer_fails_rather_than_truncating() {
    let mut buf = [0u8; AUDIO_FRAME_LEN - 1];
    let mut w = Writer::new(&mut buf);
    assert!(sample().encode(&mut w).is_err());
}
