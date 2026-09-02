//! Value noise in one, two and three dimensions.
//!
//! Integer hash plus smoothstep interpolation over a lattice. No tables beyond
//! the hash, no floats, and identical output on every target — which is the
//! requirement that rules out every off-the-shelf implementation.
//!
//! The output range is **0..1**, not -1..1. Effects overwhelmingly want a
//! brightness or a mix factor, and making the common case need no rescaling
//! saves an instruction in the hottest loop in the system.

use crate::q16::{ONE_RAW, Q16};

/// A 32-bit integer hash.
///
/// Chosen for avalanche quality on small consecutive inputs, which is exactly
/// what a lattice produces — a weak hash shows up as visible diagonal banding
/// on a strip, and that is the failure mode to avoid here.
const fn hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

/// Hash a lattice point to a value in `0..1`.
const fn lattice(x: i32, y: i32, z: i32) -> Q16 {
    let h = hash(
        (x as u32).wrapping_mul(0x9E37_79B1)
            ^ (y as u32).wrapping_mul(0x85EB_CA6B)
            ^ (z as u32).wrapping_mul(0xC2B2_AE35),
    );
    // Top 16 bits give a value in 0..1 with no bias.
    Q16((h >> 16) as i32)
}

/// Hermite fade, `3t² - 2t³`, on a value already in `0..1`.
///
/// Interpolating linearly between lattice points leaves visible creases where
/// the gradient changes abruptly. This is the cheapest fix that removes them.
const fn fade(t: Q16) -> Q16 {
    let t2 = t.mul(t);
    t2.mul(Q16::from_int(3).sub(t.mul(Q16::from_int(2))))
}

const fn split(v: Q16) -> (i32, Q16) {
    (v.0 >> 16, Q16(v.0 & (ONE_RAW - 1)))
}

/// One-dimensional value noise.
pub const fn noise1(x: Q16) -> Q16 {
    let (ix, fx) = split(x);
    let t = fade(fx);
    lattice(ix, 0, 0).lerp(lattice(ix + 1, 0, 0), t)
}

/// Two-dimensional value noise.
pub const fn noise2(x: Q16, y: Q16) -> Q16 {
    let (ix, fx) = split(x);
    let (iy, fy) = split(y);
    let tx = fade(fx);
    let ty = fade(fy);
    let a = lattice(ix, iy, 0).lerp(lattice(ix + 1, iy, 0), tx);
    let b = lattice(ix, iy + 1, 0).lerp(lattice(ix + 1, iy + 1, 0), tx);
    a.lerp(b, ty)
}

/// Three-dimensional value noise.
///
/// The one that matters most: with every LED knowing its world coordinates,
/// `noise3(x, y, z - t)` is a cloud drifting through a room, computed
/// independently on every device with no network traffic at all.
pub const fn noise3(x: Q16, y: Q16, z: Q16) -> Q16 {
    let (ix, fx) = split(x);
    let (iy, fy) = split(y);
    let (iz, fz) = split(z);
    let tx = fade(fx);
    let ty = fade(fy);
    let tz = fade(fz);

    let c00 = lattice(ix, iy, iz).lerp(lattice(ix + 1, iy, iz), tx);
    let c10 = lattice(ix, iy + 1, iz).lerp(lattice(ix + 1, iy + 1, iz), tx);
    let c01 = lattice(ix, iy, iz + 1).lerp(lattice(ix + 1, iy, iz + 1), tx);
    let c11 = lattice(ix, iy + 1, iz + 1).lerp(lattice(ix + 1, iy + 1, iz + 1), tx);

    let c0 = c00.lerp(c10, ty);
    let c1 = c01.lerp(c11, ty);
    c0.lerp(c1, tz)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_unit_range(v: Q16, what: &str) {
        assert!(
            v.0 >= 0 && v.0 <= ONE_RAW,
            "{what}: {} outside 0..1",
            v.0 as f64 / 65536.0
        );
    }

    #[test]
    fn output_stays_in_the_unit_range() {
        for i in -200i32..200 {
            let x = Q16::from_ratio(i, 7);
            in_unit_range(noise1(x), "noise1");
            in_unit_range(noise2(x, Q16::from_ratio(i, 5)), "noise2");
            in_unit_range(
                noise3(x, Q16::from_ratio(i, 5), Q16::from_ratio(i, 3)),
                "noise3",
            );
        }
    }

    #[test]
    fn it_is_deterministic() {
        // The whole point. Two devices rendering the same effect must agree.
        let x = Q16::from_ratio(7, 3);
        assert_eq!(noise1(x), noise1(x));
        assert_eq!(noise2(x, x), noise2(x, x));
        assert_eq!(noise3(x, x, x), noise3(x, x, x));
    }

    #[test]
    fn lattice_points_are_exact_and_stable() {
        // At integer coordinates the fade is zero, so the value is the lattice
        // hash itself, with no interpolation error.
        for i in 0..8 {
            let v = noise1(Q16::from_int(i));
            assert_eq!(v, lattice(i as i32, 0, 0));
        }
    }

    #[test]
    fn neighbouring_lattice_points_differ() {
        // A hash that collided on consecutive inputs would show as flat bands.
        let mut seen_difference = 0;
        for i in 0..64 {
            if lattice(i, 0, 0) != lattice(i + 1, 0, 0) {
                seen_difference += 1;
            }
        }
        assert_eq!(seen_difference, 64, "consecutive lattice points collided");
    }

    #[test]
    fn it_is_continuous_between_lattice_points() {
        // Sampled finely, successive values must not jump: a discontinuity is a
        // visible seam on a strip.
        let mut prev = noise1(Q16::ZERO);
        for i in 1..=256 {
            let v = noise1(Q16::from_ratio(i, 256));
            let jump = (v.0 - prev.0).abs();
            assert!(jump < ONE_RAW / 4, "jump of {jump} at step {i}");
            prev = v;
        }
    }

    #[test]
    fn dimensions_are_independent() {
        // Moving in y must change noise2, or the second dimension is decorative.
        let a = noise2(Q16::from_ratio(1, 2), Q16::from_ratio(1, 2));
        let b = noise2(Q16::from_ratio(1, 2), Q16::from_ratio(3, 2));
        assert_ne!(a, b);

        let c = noise3(Q16::HALF, Q16::HALF, Q16::HALF);
        let d = noise3(Q16::HALF, Q16::HALF, Q16::from_ratio(3, 2));
        assert_ne!(c, d);
    }

    #[test]
    fn negative_coordinates_work() {
        // Effects routinely evaluate noise at `z - t`, which goes negative as
        // soon as the show starts.
        in_unit_range(noise1(Q16::from_int(-5)), "noise1 negative");
        in_unit_range(
            noise3(Q16::from_int(-5), Q16::from_int(-9), Q16::from_int(-1)),
            "noise3",
        );
        assert_ne!(noise1(Q16::from_int(-5)), noise1(Q16::from_int(-4)));
    }

    #[test]
    fn fade_is_smooth_and_hits_both_ends() {
        assert_eq!(fade(Q16::ZERO), Q16::ZERO);
        assert_eq!(fade(Q16::ONE), Q16::ONE);
        assert_eq!(fade(Q16::HALF), Q16::HALF);
    }

    #[test]
    fn the_hash_avalanches() {
        // Consecutive inputs must not produce consecutive outputs, or the noise
        // shows visible diagonal banding.
        let a = hash(1000);
        let b = hash(1001);
        let diff = a ^ b;
        assert!(
            diff.count_ones() > 8,
            "only {} bits changed between neighbours",
            diff.count_ones()
        );
    }
}
