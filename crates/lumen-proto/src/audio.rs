//! The audio channel payload.
//!
//! Analysis happens at the source, once, and only the result is broadcast —
//! never raw audio. This is the shape of that result: what an analyser puts
//! into a `CHAN` payload and what every device reads out of it.
//!
//! It lives here, on the permissive side, rather than beside the analyser in
//! `lumen-device`. Publishing audio is *talking to* the mesh, not *being part
//! of* it: a desktop app, a phone or a dedicated line-in box should be able to
//! feed a mesh without taking on the GPL, and the four sources named in
//! `docs/effects.md` include exactly those.
//!
//! # Layout
//!
//! 40 bytes, little-endian like everything else:
//!
//! ```text
//! 0   32  bands[32]     u8 each, AGC-normalised
//! 32  1   level         u8
//! 33  1   smoothed      u8
//! 34  1   flags         bit0 onset, rest reserved
//! 35  1   confidence    u8
//! 36  2   beat_phase    u16, 0..65535 spanning one beat
//! 38  2   bpm_x4        u16, quarter-BPM; 0 means unknown
//! ```
//!
//! `beat_phase` is a `u16` rather than a `q16` because it is a fraction of a
//! beat and nothing else: it wraps, it has no integer part, and a `u16` wraps
//! on its own at exactly the right place. That also makes the field
//! self-describing — every bit pattern is a valid phase, so there is nothing to
//! validate and no malformed case to define.
//!
//! Publishing *phase* rather than beat events is what makes this survive a
//! lossy network: a receiver that misses a packet extrapolates where in the bar
//! it is instead of stuttering, and a beat that arrives late as an event is
//! worse than useless because the flash lands after the drum.

use crate::buf::{Reader, Writer};
use crate::error::{DecodeError, EncodeError};

type DResult<T> = Result<T, DecodeError>;
type EResult = Result<(), EncodeError>;

/// Band count, fixed by the published layout.
pub const BANDS: usize = 32;

/// Encoded size of one frame.
pub const AUDIO_FRAME_LEN: usize = BANDS + 8;

/// `flags` bit 0: this frame is an onset.
pub const FLAG_ONSET: u8 = 1 << 0;

/// One analysed window, as it travels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AudioFrame {
    /// Log-spaced band magnitudes, AGC-normalised so quiet music still fills
    /// the range.
    pub bands: [u8; BANDS],
    /// Overall level now.
    pub level: u8,
    /// Overall level, smoothed, for anything that should not flicker.
    pub smoothed_level: u8,
    /// Whether this frame is an onset.
    pub onset: bool,
    /// Position within the beat: 0 at the beat, wrapping at one turn.
    pub beat_phase: u16,
    /// Tempo in quarter-BPM, so 120.25 BPM is 481. Zero means unknown.
    pub bpm_x4: u16,
    /// How much to trust `beat_phase` and `bpm_x4`.
    pub confidence: u8,
}

impl Default for AudioFrame {
    fn default() -> Self {
        AudioFrame {
            bands: [0; BANDS],
            level: 0,
            smoothed_level: 0,
            onset: false,
            beat_phase: 0,
            bpm_x4: 0,
            confidence: 0,
        }
    }
}

impl AudioFrame {
    pub fn decode(r: &mut Reader<'_>) -> DResult<AudioFrame> {
        let mut bands = [0u8; BANDS];
        bands.copy_from_slice(r.bytes(BANDS)?);
        let level = r.u8()?;
        let smoothed_level = r.u8()?;
        let flags = r.u8()?;
        let confidence = r.u8()?;
        let beat_phase = r.u16()?;
        let bpm_x4 = r.u16()?;
        Ok(AudioFrame {
            bands,
            level,
            smoothed_level,
            // Unknown flag bits are ignored, not rejected — that rule is what
            // lets a later minor version add one without breaking every device
            // already deployed.
            onset: flags & FLAG_ONSET != 0,
            beat_phase,
            bpm_x4,
            confidence,
        })
    }

    pub fn encode(&self, w: &mut Writer<'_>) -> EResult {
        w.bytes(&self.bands)?;
        w.u8(self.level)?;
        w.u8(self.smoothed_level)?;
        w.u8(if self.onset { FLAG_ONSET } else { 0 })?;
        w.u8(self.confidence)?;
        w.u16(self.beat_phase)?;
        w.u16(self.bpm_x4)
    }

    /// Read a frame straight from a `CHAN` payload.
    pub fn from_payload(payload: &[u8]) -> DResult<AudioFrame> {
        if payload.len() < AUDIO_FRAME_LEN {
            return Err(DecodeError::Truncated);
        }
        // Trailing bytes are ignored rather than refused: a later minor version
        // may append a field, and a device that rejected the whole frame for
        // that would go dark on an upgrade it did not need.
        AudioFrame::decode(&mut Reader::new(&payload[..AUDIO_FRAME_LEN]))
    }

    /// The tempo in whole BPM, rounded down. Zero when unknown.
    pub fn bpm(&self) -> u16 {
        self.bpm_x4 / 4
    }

    /// One band, or zero if `index` is past the end.
    ///
    /// Out of range reads as silence rather than panicking: the index reaching
    /// this comes from a compiled program, and a device must not die because an
    /// effect asked for a band that does not exist.
    pub fn band(&self, index: usize) -> u8 {
        self.bands.get(index).copied().unwrap_or(0)
    }
}

#[cfg(test)]
#[path = "audio_tests.rs"]
mod tests;
