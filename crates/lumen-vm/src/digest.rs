//! A fingerprint for one rendered frame.
//!
//! The architecture's central claim is that every device computes the same show
//! from the same clock — that is why the VM is fixed point, why the dither is
//! ordered rather than random, and why a two-core render is checked against a
//! one-core render byte for byte. All of that has been asserted in tests and
//! never once checked against a device.
//!
//! This is how it gets checked. A device hashes the frame it rendered and says
//! so; a host renders the same program at the same show time and hashes it too.
//! Equal hashes mean identical frames. Unequal ones mean a bug in something that
//! ships, and the milestone that says "host and device produce identical output
//! for the same program and time" stops being a promise.
//!
//! # Why a hash rather than the pixels
//!
//! 300 LEDs is 900 bytes a frame. Sending that at 30 fps to prove a point would
//! cost more bandwidth than the show does, and a device with 40 KB of RAM should
//! not be buffering frames to report on them. Eight bytes says the same thing
//! for anything short of a deliberate collision.
//!
//! # FNV-1a, and why not something stronger
//!
//! This detects **accident**: a rounding rule that differs, a register clobbered
//! on one chip, a dither keyed on the wrong clock. It is not defending against
//! anyone, so a hash that is four lines and has no tables is the right size of
//! tool. Signing is a different problem, lives in `lumen-crypto`, and protects
//! programs rather than frames.

use crate::q16::Q16;

const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// A running fingerprint of rendered output.
///
/// Fed the linear values rather than the encoded bytes, so it compares what the
/// *VM* produced. Two devices with different supplies derate differently and
/// would disagree on the codes while agreeing perfectly on the render, which is
/// the thing under test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Digest(u64);

impl Default for Digest {
    fn default() -> Digest {
        Digest::new()
    }
}

impl Digest {
    pub const fn new() -> Digest {
        Digest(OFFSET)
    }

    /// Fold in one Q16 value.
    ///
    /// Little-endian byte order, fixed here rather than taken from the target:
    /// the whole point is that two different chips agree, and a hash that read
    /// the host's endianness would disagree for a reason that has nothing to do
    /// with rendering.
    pub fn push(&mut self, v: Q16) {
        for byte in v.0.to_le_bytes() {
            self.0 ^= byte as u64;
            self.0 = self.0.wrapping_mul(PRIME);
        }
    }

    /// Fold in a whole frame, three values per LED.
    pub fn push_frame(&mut self, linear: &[Q16]) {
        for v in linear {
            self.push(*v);
        }
    }

    /// The fingerprint so far.
    pub const fn value(self) -> u64 {
        self.0
    }

    /// One frame's fingerprint, from nothing.
    pub fn of_frame(linear: &[Q16]) -> u64 {
        let mut d = Digest::new();
        d.push_frame(linear);
        d.value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_frame_hashes_the_same_way() {
        let frame = [Q16::ONE, Q16::HALF, Q16::ZERO, Q16(1234)];
        assert_eq!(Digest::of_frame(&frame), Digest::of_frame(&frame));
    }

    #[test]
    fn one_pixel_changing_changes_the_hash() {
        // The failure this exists to catch is a single pixel differing between a
        // host and a device, so a hash that missed one would be worthless.
        let mut frame = [Q16::ZERO; 90];
        let before = Digest::of_frame(&frame);
        frame[47] = Q16(1);
        assert_ne!(before, Digest::of_frame(&frame));
    }

    #[test]
    fn order_matters() {
        // Two pixels swapping is a real bug - a shard rendering its range
        // backwards - and a hash that summed would not see it.
        let a = [Q16::ONE, Q16::ZERO];
        let b = [Q16::ZERO, Q16::ONE];
        assert_ne!(Digest::of_frame(&a), Digest::of_frame(&b));
    }

    #[test]
    fn a_negative_value_is_not_confused_with_a_positive_one() {
        // `prev` can carry a negative through a frame, and a hash that folded
        // the absolute value would let a sign flip through.
        assert_ne!(Digest::of_frame(&[Q16(-1)]), Digest::of_frame(&[Q16(1)]));
    }

    #[test]
    fn an_empty_frame_is_not_zero() {
        // A device that failed to render and reported a zeroed hash would look
        // like agreement with a host that also reported zero. The offset basis
        // is what stops "nothing happened" from being a valid-looking answer.
        assert_ne!(Digest::of_frame(&[]), 0);
        assert_eq!(Digest::of_frame(&[]), OFFSET);
    }

    #[test]
    fn pushing_in_pieces_matches_pushing_at_once() {
        // A device hashes as it writes each pixel; a host hashes a whole buffer.
        // They have to agree or the comparison is measuring the hash rather than
        // the render.
        let frame = [Q16::ONE, Q16::HALF, Q16(-9), Q16(77), Q16::ZERO, Q16(3)];
        let mut piecemeal = Digest::new();
        for v in &frame {
            piecemeal.push(*v);
        }
        assert_eq!(piecemeal.value(), Digest::of_frame(&frame));
    }
}
