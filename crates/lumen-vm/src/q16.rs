//! Q16.16 fixed point.
//!
//! Every value the VM computes is one of these. Fixed point rather than `f32`
//! for one reason that outranks the others: **a program must produce
//! bit-identical output on every chip in the mesh.** Two devices lighting the
//! same strip from the same program cannot disagree about a pixel, and `f32` on
//! three different targets — with three different compilers and three different
//! rounding habits — does not give that guarantee. Neither the ESP32-S3 nor the
//! RP2040 has a useful FPU for this workload anyway.
//!
//! Arithmetic **saturates rather than wraps**. A wrapped brightness is a bright
//! flash where an author wrote a slow fade, which is a visible artefact from an
//! invisible cause. Saturation clamps to the end of the range and stays there,
//! which is the wrong answer in a way you can see and debug. Division by zero is
//! the exception: it is a [`Fault`], because there is no clamped answer that
//! could be right.

use crate::tables::{ATAN_TABLE, EXP2_FRACTION, LOG2_MANTISSA, SIN_QUARTER};
use crate::Fault;

/// A Q16.16 fixed-point scalar: value = `raw / 65536`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash, Debug)]
pub struct Q16(pub i32);

/// Fractional bits.
pub const SHIFT: u32 = 16;
/// `1.0`, as a raw value.
pub const ONE_RAW: i32 = 1 << SHIFT;

impl Q16 {
    pub const ZERO: Q16 = Q16(0);
    pub const ONE: Q16 = Q16(ONE_RAW);
    pub const HALF: Q16 = Q16(ONE_RAW / 2);
    pub const MIN: Q16 = Q16(i32::MIN);
    pub const MAX: Q16 = Q16(i32::MAX);

    /// π, to the nearest representable value.
    pub const PI: Q16 = Q16(205_887);
    /// 2π.
    pub const TAU: Q16 = Q16(411_775);

    /// From a whole number of units.
    pub const fn from_int(v: i16) -> Q16 {
        Q16((v as i32) << SHIFT)
    }

    /// From a ratio, rounding to nearest. `from_ratio(1, 3)` is a third.
    ///
    /// Returns [`Q16::ZERO`] for a zero denominator; this is a const helper for
    /// building constants, not an arithmetic instruction, and a constant pool
    /// with a division by zero in it is a compiler bug rather than a runtime
    /// condition.
    pub const fn from_ratio(num: i32, den: i32) -> Q16 {
        if den == 0 {
            return Q16::ZERO;
        }
        Q16((((num as i64) << SHIFT) / (den as i64)) as i32)
    }

    /// Truncate toward zero.
    pub const fn to_int(self) -> i32 {
        self.0 >> SHIFT
    }

    /// The largest integer not greater than this value.
    pub const fn floor(self) -> Q16 {
        Q16(self.0 & !(ONE_RAW - 1))
    }

    /// The fractional part, always in `0..1` — including for negatives, where
    /// `fract(-0.25)` is `0.75`. Effects index palettes and noise lattices with
    /// this, and a negative fraction would read off the wrong end.
    pub const fn fract(self) -> Q16 {
        Q16(self.0 & (ONE_RAW - 1))
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub const fn add(self, o: Q16) -> Q16 {
        Q16(self.0.saturating_add(o.0))
    }

    pub const fn sub(self, o: Q16) -> Q16 {
        Q16(self.0.saturating_sub(o.0))
    }

    pub const fn neg(self) -> Q16 {
        // `-i32::MIN` overflows; saturating_neg pins it to MAX.
        Q16(self.0.saturating_neg())
    }

    pub const fn abs(self) -> Q16 {
        Q16(self.0.saturating_abs())
    }

    /// Multiply, rounding toward zero, saturating on overflow.
    pub const fn mul(self, o: Q16) -> Q16 {
        let wide = (self.0 as i64) * (o.0 as i64);
        Q16(saturate(wide >> SHIFT))
    }

    /// Divide. Division by zero is a [`Fault::DivideByZero`] — there is no
    /// clamped answer that could be correct, so the program stops rather than
    /// rendering a lie.
    pub const fn div(self, o: Q16) -> Result<Q16, Fault> {
        if o.0 == 0 {
            return Err(Fault::DivideByZero);
        }
        let wide = ((self.0 as i64) << SHIFT) / (o.0 as i64);
        Ok(Q16(saturate(wide)))
    }

    /// `self * a + b`, with the product kept at full width before the add.
    ///
    /// Not just a convenience: rounding once instead of twice is why a chain of
    /// these does not drift.
    pub const fn madd(self, a: Q16, b: Q16) -> Q16 {
        let wide = (((self.0 as i64) * (a.0 as i64)) >> SHIFT) + (b.0 as i64);
        Q16(saturate(wide))
    }

    pub const fn min(self, o: Q16) -> Q16 {
        if self.0 < o.0 {
            self
        } else {
            o
        }
    }

    pub const fn max(self, o: Q16) -> Q16 {
        if self.0 > o.0 {
            self
        } else {
            o
        }
    }

    /// Clamp into `[lo, hi]`. With `lo > hi` the result is `lo`, matching the
    /// order the operations are applied in rather than inventing an error.
    pub const fn clamp(self, lo: Q16, hi: Q16) -> Q16 {
        self.min(hi).max(lo)
    }

    /// Linear interpolation. `t` outside `0..1` extrapolates.
    pub const fn lerp(self, to: Q16, t: Q16) -> Q16 {
        to.sub(self).mul(t).add(self)
    }

    /// `0` below `edge`, `1` at or above it.
    pub const fn step(self, edge: Q16) -> Q16 {
        if self.0 >= edge.0 {
            Q16::ONE
        } else {
            Q16::ZERO
        }
    }

    /// Hermite interpolation between two edges: `3t² - 2t³`.
    pub const fn smoothstep(self, e0: Q16, e1: Q16) -> Q16 {
        if e1.0 == e0.0 {
            return self.step(e0);
        }
        let span = e1.sub(e0);
        // The span is non-zero, so this division cannot fault.
        let t = match self.sub(e0).div(span) {
            Ok(v) => v.clamp(Q16::ZERO, Q16::ONE),
            Err(_) => return Q16::ZERO,
        };
        let t2 = t.mul(t);
        let three_minus_two_t = Q16::from_int(3).sub(t.mul(Q16::from_int(2)));
        t2.mul(three_minus_two_t)
    }

    /// Square root. Negative inputs are a [`Fault::DomainError`].
    pub const fn sqrt(self) -> Result<Q16, Fault> {
        if self.0 < 0 {
            return Err(Fault::DomainError);
        }
        // sqrt(x/2^16) * 2^16 = sqrt(x * 2^16), computed on the widened value.
        Ok(Q16(isqrt64((self.0 as i64) << SHIFT) as i32))
    }

    /// Sine of an angle in **turns**, where `1.0` is a full circle.
    ///
    /// The primitive the table is built for. [`Q16::sin`] wraps it for radians.
    /// Effects that want a cycle per second are better off in turns anyway —
    /// `sin_turns(t)` needs no 2π anywhere.
    pub fn sin_turns(self) -> Q16 {
        // Reduce to 0..1, then to one of four quadrants of a 1024-step circle.
        let frac = self.fract().0 as u32; // 0..65535
        let pos = ((frac as u64) * 1024) >> SHIFT; // 0..1023
        let idx = (pos & 0x3FF) as usize;
        // Sub-step position, for interpolation between table entries.
        let sub = (((frac as u64) * 1024) & 0xFFFF) as i64;

        let (i, negate) = match idx {
            0..=255 => (idx, false),
            256..=511 => (512 - idx, false),
            512..=767 => (idx - 512, true),
            _ => (1024 - idx, true),
        };
        // Mirrored quadrants walk the table backwards, so the neighbour to
        // interpolate toward is the previous entry rather than the next.
        let descending = matches!(idx, 256..=511 | 768..=1023);
        let a = SIN_QUARTER[i] as i64;
        let b = if descending {
            SIN_QUARTER[i.saturating_sub(1)] as i64
        } else {
            SIN_QUARTER[i + 1] as i64
        };
        let v = a + (((b - a) * sub) >> SHIFT);
        let v = if negate { -v } else { v };
        Q16(saturate(v))
    }

    /// Cosine in turns.
    pub fn cos_turns(self) -> Q16 {
        self.add(Q16(ONE_RAW / 4)).sin_turns()
    }

    /// Sine of an angle in radians.
    pub fn sin(self) -> Q16 {
        match self.div(Q16::TAU) {
            Ok(turns) => turns.sin_turns(),
            // TAU is a non-zero constant, so this is unreachable.
            Err(_) => Q16::ZERO,
        }
    }

    /// Cosine of an angle in radians.
    pub fn cos(self) -> Q16 {
        match self.div(Q16::TAU) {
            Ok(turns) => turns.cos_turns(),
            Err(_) => Q16::ONE,
        }
    }

    /// Base-2 logarithm. Zero or negative is a [`Fault::DomainError`].
    pub fn log2(self) -> Result<Q16, Fault> {
        if self.0 <= 0 {
            return Err(Fault::DomainError);
        }
        let raw = self.0 as u32;
        // Split into 2^e * (1 + m) with m in 0..1, then table-look-up the
        // mantissa. `31 - leading_zeros` is the position of the top set bit.
        let top = 31 - raw.leading_zeros(); // 0..=30
        let exponent = top as i32 - SHIFT as i32;
        // Normalise so the top bit sits at position SHIFT, giving 1.0..2.0.
        let mantissa = if top >= SHIFT {
            raw >> (top - SHIFT)
        } else {
            raw << (SHIFT - top)
        };
        let m = (mantissa - ONE_RAW as u32) as u64; // 0..65535
        let idx = (m >> 8) as usize; // 0..255
        let sub = (m & 0xFF) as i64;
        let a = LOG2_MANTISSA[idx] as i64;
        let b = LOG2_MANTISSA[idx + 1] as i64;
        let frac = a + (((b - a) * sub) >> 8);
        Ok(Q16(saturate(((exponent as i64) << SHIFT) + frac)))
    }

    /// Natural logarithm.
    pub fn ln(self) -> Result<Q16, Fault> {
        // ln(x) = log2(x) * ln(2)
        const LN2: Q16 = Q16(45_426);
        Ok(self.log2()?.mul(LN2))
    }

    /// `2^self`. Saturates well outside the representable range rather than
    /// wrapping to a small number, which would look like a black pixel in the
    /// middle of a bright ramp.
    pub fn exp2(self) -> Q16 {
        if self.0 >= (15 << SHIFT) {
            return Q16::MAX;
        }
        if self.0 <= -(16 << SHIFT) {
            return Q16::ZERO;
        }
        let int_part = self.floor().to_int();
        let frac = self.fract().0 as u64; // 0..65535
        let idx = (frac >> 8) as usize;
        let sub = (frac & 0xFF) as i64;
        let a = EXP2_FRACTION[idx] as i64;
        let b = EXP2_FRACTION[idx + 1] as i64;
        let f = a + (((b - a) * sub) >> 8);
        let base = (ONE_RAW as i64) + f; // 1.0 ..= 2.0 in Q16
        let scaled = if int_part >= 0 {
            base << int_part
        } else {
            base >> (-int_part)
        };
        Q16(saturate(scaled))
    }

    /// `e^self`.
    pub fn exp(self) -> Q16 {
        // e^x = 2^(x * log2(e))
        const LOG2_E: Q16 = Q16(94_548);
        self.mul(LOG2_E).exp2()
    }

    /// `self^exponent`, for a positive base.
    ///
    /// Zero to any positive power is zero; a zero or negative base with a
    /// non-integer exponent is a [`Fault::DomainError`], because there is no
    /// real answer and silently returning zero would hide an authoring mistake.
    pub fn pow(self, exponent: Q16) -> Result<Q16, Fault> {
        if self.0 == 0 {
            return Ok(if exponent.0 > 0 { Q16::ZERO } else { Q16::ONE });
        }
        if self.0 < 0 {
            return Err(Fault::DomainError);
        }
        Ok(self.log2()?.mul(exponent).exp2())
    }

    /// Four-quadrant arc tangent, in radians, in `(-π, π]`.
    pub fn atan2(y: Q16, x: Q16) -> Q16 {
        if x.0 == 0 && y.0 == 0 {
            return Q16::ZERO;
        }
        let ax = y.0.saturating_abs() as i64;
        let ay = x.0.saturating_abs() as i64;
        // Look up atan of the smaller over the larger, so the ratio stays in
        // 0..1 and one table covers every octant.
        let (num, den, swapped) = if ax <= ay {
            (ax, ay, false)
        } else {
            (ay, ax, true)
        };
        let ratio = if den == 0 { 0 } else { (num << SHIFT) / den };
        let idx = ((ratio * 256) >> SHIFT).clamp(0, 255) as usize;
        let sub = (ratio * 256) & 0xFFFF;
        let a = ATAN_TABLE[idx] as i64;
        let b = ATAN_TABLE[idx + 1] as i64;
        let mut angle = a + (((b - a) * sub) >> SHIFT);
        if swapped {
            angle = (Q16::PI.0 as i64 / 2) - angle;
        }
        let mut out = angle;
        if x.0 < 0 {
            out = (Q16::PI.0 as i64) - out;
        }
        if y.0 < 0 {
            out = -out;
        }
        Q16(saturate(out))
    }

    /// Euclidean length of a 2D vector.
    pub fn len2(x: Q16, y: Q16) -> Q16 {
        // Widen before squaring: two values near 1.0 would otherwise overflow
        // the Q16 range on the way to a perfectly representable answer.
        let sum = ((x.0 as i64 * x.0 as i64) >> SHIFT) + ((y.0 as i64 * y.0 as i64) >> SHIFT);
        Q16(isqrt64(sum << SHIFT) as i32)
    }

    /// Euclidean length of a 3D vector.
    pub fn len3(x: Q16, y: Q16, z: Q16) -> Q16 {
        let sum = ((x.0 as i64 * x.0 as i64) >> SHIFT)
            + ((y.0 as i64 * y.0 as i64) >> SHIFT)
            + ((z.0 as i64 * z.0 as i64) >> SHIFT);
        Q16(isqrt64(sum << SHIFT) as i32)
    }
}

/// Clamp a widened intermediate back into `i32`, saturating.
const fn saturate(v: i64) -> i32 {
    if v > i32::MAX as i64 {
        i32::MAX
    } else if v < i32::MIN as i64 {
        i32::MIN
    } else {
        v as i32
    }
}

/// Integer square root by binary digit-by-digit extraction.
///
/// No floats, no iteration count that depends on the value, and identical on
/// every target — which is the whole requirement.
const fn isqrt64(v: i64) -> i64 {
    if v <= 0 {
        return 0;
    }
    let mut rem: i64 = v;
    let mut root: i64 = 0;
    let mut bit: i64 = 1 << 62;
    while bit > rem {
        bit >>= 2;
    }
    while bit != 0 {
        if rem >= root + bit {
            rem -= root + bit;
            root = (root >> 1) + bit;
        } else {
            root >>= 1;
        }
        bit >>= 2;
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert two Q16 values agree to within `tol` raw units.
    fn close(a: Q16, b: f64, tol: i32, what: &str) {
        let expect = (b * 65536.0).round() as i64;
        let diff = (a.0 as i64 - expect).abs();
        assert!(
            diff <= tol as i64,
            "{what}: got {} ({}), expected {} ({b}), diff {diff} > {tol}",
            a.0,
            a.0 as f64 / 65536.0,
            expect
        );
    }

    #[test]
    fn constants_are_what_they_claim() {
        assert_eq!(Q16::ONE.0, 65536);
        assert_eq!(Q16::from_int(1), Q16::ONE);
        assert_eq!(Q16::from_int(-3).to_int(), -3);
        close(Q16::PI, core::f64::consts::PI, 1, "PI");
        close(Q16::TAU, core::f64::consts::TAU, 1, "TAU");
        assert_eq!(Q16::HALF.0, 32768);
    }

    #[test]
    fn from_ratio_rounds_and_survives_a_zero_denominator() {
        close(Q16::from_ratio(1, 3), 1.0 / 3.0, 1, "1/3");
        close(Q16::from_ratio(-1, 4), -0.25, 1, "-1/4");
        assert_eq!(Q16::from_ratio(1, 0), Q16::ZERO);
    }

    #[test]
    fn floor_and_fract_split_a_value_and_put_it_back() {
        for raw in [0, 1, 65535, 65536, 100_000, -1, -65536, -100_000] {
            let v = Q16(raw);
            assert_eq!(
                v.floor().add(v.fract()),
                v,
                "floor+fract must reconstruct {raw}"
            );
            assert!(v.fract().0 >= 0, "fract must never be negative");
            assert!(v.fract().0 < ONE_RAW);
        }
        // The case that catches a naive implementation: a negative value.
        close(Q16(-ONE_RAW / 4).fract(), 0.75, 1, "fract(-0.25)");
    }

    #[test]
    fn arithmetic_saturates_instead_of_wrapping() {
        // A wrapped brightness is a bright flash where the author wrote a fade.
        assert_eq!(Q16::MAX.add(Q16::ONE), Q16::MAX);
        assert_eq!(Q16::MIN.sub(Q16::ONE), Q16::MIN);
        assert_eq!(Q16::MAX.mul(Q16::from_int(1000)), Q16::MAX);
        assert_eq!(Q16::MIN.mul(Q16::from_int(1000)), Q16::MIN);
        assert_eq!(Q16::MIN.neg(), Q16::MAX);
        assert_eq!(Q16::MIN.abs(), Q16::MAX);
    }

    #[test]
    fn multiplication_is_exact_for_representable_values() {
        close(Q16::HALF.mul(Q16::HALF), 0.25, 0, "0.5*0.5");
        close(Q16::from_int(3).mul(Q16::from_int(4)), 12.0, 0, "3*4");
        close(Q16::from_int(-3).mul(Q16::HALF), -1.5, 0, "-3*0.5");
        assert_eq!(Q16::ZERO.mul(Q16::MAX), Q16::ZERO);
    }

    #[test]
    fn division_by_zero_is_a_fault_not_a_clamp() {
        // There is no clamped answer that could be right, so the program stops.
        assert_eq!(Q16::ONE.div(Q16::ZERO), Err(Fault::DivideByZero));
        assert_eq!(Q16::ZERO.div(Q16::ZERO), Err(Fault::DivideByZero));
        close(Q16::ONE.div(Q16::from_int(4)).unwrap(), 0.25, 1, "1/4");
        close(
            Q16::from_int(-6).div(Q16::from_int(4)).unwrap(),
            -1.5,
            1,
            "-6/4",
        );
    }

    #[test]
    fn madd_rounds_once_not_twice() {
        let a = Q16::from_ratio(1, 3);
        let chained = a.mul(a).add(a);
        let fused = a.madd(a, a);
        // Both are close to the true value; the fused form is never worse.
        let truth = (1.0 / 3.0) * (1.0 / 3.0) + (1.0 / 3.0);
        let e_chained = ((chained.0 as f64 / 65536.0) - truth).abs();
        let e_fused = ((fused.0 as f64 / 65536.0) - truth).abs();
        assert!(
            e_fused <= e_chained,
            "fused {e_fused} worse than {e_chained}"
        );
    }

    #[test]
    fn min_max_and_clamp_behave_at_the_edges() {
        let lo = Q16::from_int(2);
        let hi = Q16::from_int(5);
        assert_eq!(Q16::from_int(1).clamp(lo, hi), lo);
        assert_eq!(Q16::from_int(9).clamp(lo, hi), hi);
        assert_eq!(Q16::from_int(3).clamp(lo, hi), Q16::from_int(3));
        assert_eq!(lo.min(hi), lo);
        assert_eq!(lo.max(hi), hi);
        // Inverted bounds resolve to `lo` rather than erroring.
        assert_eq!(Q16::from_int(3).clamp(hi, lo), hi);
    }

    #[test]
    fn lerp_hits_both_ends_exactly() {
        let a = Q16::from_int(10);
        let b = Q16::from_int(20);
        assert_eq!(a.lerp(b, Q16::ZERO), a);
        assert_eq!(a.lerp(b, Q16::ONE), b);
        close(a.lerp(b, Q16::HALF), 15.0, 1, "midpoint");
    }

    #[test]
    fn step_and_smoothstep() {
        assert_eq!(Q16::from_int(1).step(Q16::from_int(2)), Q16::ZERO);
        assert_eq!(Q16::from_int(2).step(Q16::from_int(2)), Q16::ONE);

        let e0 = Q16::ZERO;
        let e1 = Q16::ONE;
        assert_eq!(Q16(-1000).smoothstep(e0, e1), Q16::ZERO);
        assert_eq!(Q16::from_int(2).smoothstep(e0, e1), Q16::ONE);
        close(Q16::HALF.smoothstep(e0, e1), 0.5, 8, "smoothstep midpoint");
        // Degenerate edges fall back to a hard step rather than dividing by zero.
        assert_eq!(Q16::ONE.smoothstep(e1, e1), Q16::ONE);
        assert_eq!(Q16::ZERO.smoothstep(e1, e1), Q16::ZERO);
    }

    #[test]
    fn sqrt_matches_the_real_thing() {
        for v in [0.0f64, 0.25, 1.0, 2.0, 9.0, 100.0, 1000.0] {
            let q = Q16((v * 65536.0) as i32);
            close(q.sqrt().unwrap(), v.sqrt(), 4, "sqrt");
        }
        assert_eq!(Q16::from_int(-1).sqrt(), Err(Fault::DomainError));
    }

    #[test]
    fn sin_turns_is_accurate_across_a_full_circle() {
        // Sampled every 1/256 turn, including past the ends to prove wrapping.
        for i in -300i32..=600 {
            let turns = i as f64 / 256.0;
            let q = Q16((turns * 65536.0).round() as i32);
            close(
                q.sin_turns(),
                (turns * core::f64::consts::TAU).sin(),
                160,
                "sin_turns",
            );
        }
    }

    #[test]
    fn sin_turns_hits_the_cardinal_points() {
        close(Q16::ZERO.sin_turns(), 0.0, 2, "sin(0)");
        close(Q16::from_ratio(1, 4).sin_turns(), 1.0, 2, "sin(quarter)");
        close(Q16::from_ratio(1, 2).sin_turns(), 0.0, 8, "sin(half)");
        close(
            Q16::from_ratio(3, 4).sin_turns(),
            -1.0,
            2,
            "sin(three quarter)",
        );
        close(Q16::ONE.sin_turns(), 0.0, 2, "sin(full)");
    }

    #[test]
    fn cos_leads_sin_by_a_quarter_turn() {
        for i in 0..64 {
            let q = Q16::from_ratio(i, 64);
            let expect = q.add(Q16::from_ratio(1, 4)).sin_turns();
            assert_eq!(q.cos_turns(), expect);
        }
    }

    #[test]
    fn radian_sin_and_cos_agree_with_the_real_thing() {
        for i in -20i32..=20 {
            let rad = i as f64 / 3.0;
            let q = Q16((rad * 65536.0).round() as i32);
            close(q.sin(), rad.sin(), 300, "sin(rad)");
            close(q.cos(), rad.cos(), 300, "cos(rad)");
        }
    }

    #[test]
    fn log2_covers_the_representable_range() {
        for v in [0.001f64, 0.5, 1.0, 1.5, 2.0, 3.0, 16.0, 1000.0, 30000.0] {
            let q = Q16((v * 65536.0).round() as i32);
            // Compare against the log of the value Q16 can actually hold. Near
            // zero the quantisation dominates: 0.001 stores as 0.0010071, whose
            // log2 differs from log2(0.001) by far more than the table error.
            let representable = q.0 as f64 / 65536.0;
            close(q.log2().unwrap(), representable.log2(), 40, "log2");
        }
        assert_eq!(Q16::ZERO.log2(), Err(Fault::DomainError));
        assert_eq!(Q16::from_int(-2).log2(), Err(Fault::DomainError));
    }

    #[test]
    fn ln_matches_the_real_thing() {
        for v in [0.5f64, 1.0, core::f64::consts::E, 10.0] {
            let q = Q16((v * 65536.0).round() as i32);
            close(q.ln().unwrap(), v.ln(), 60, "ln");
        }
        assert_eq!(Q16::ZERO.ln(), Err(Fault::DomainError));
    }

    #[test]
    fn exp2_matches_the_real_thing_and_saturates_at_the_ends() {
        for v in [-8.0f64, -1.0, 0.0, 0.5, 1.0, 3.5, 10.0] {
            let q = Q16((v * 65536.0).round() as i32);
            close(q.exp2(), 2f64.powf(v), 300, "exp2");
        }
        assert_eq!(Q16::from_int(100).exp2(), Q16::MAX);
        assert_eq!(Q16::from_int(-100).exp2(), Q16::ZERO);
    }

    #[test]
    fn exp_matches_the_real_thing() {
        for v in [-2.0f64, 0.0, 1.0, 5.0] {
            let q = Q16((v * 65536.0).round() as i32);
            close(q.exp(), v.exp(), 800, "exp");
        }
    }

    #[test]
    fn exp2_and_log2_are_inverses() {
        for v in [0.25f64, 1.0, 3.0, 50.0, 1000.0] {
            let q = Q16((v * 65536.0).round() as i32);
            let back = q.log2().unwrap().exp2();
            let rel = ((back.0 as f64 / 65536.0) - v).abs() / v;
            assert!(
                rel < 0.01,
                "round trip of {v} gave {}",
                back.0 as f64 / 65536.0
            );
        }
    }

    #[test]
    fn pow_handles_its_special_cases() {
        close(
            Q16::from_int(2).pow(Q16::from_int(10)).unwrap(),
            1024.0,
            4000,
            "2^10",
        );
        close(Q16::from_int(9).pow(Q16::HALF).unwrap(), 3.0, 200, "9^0.5");
        // Zero to a positive power is zero; to a non-positive power, one.
        assert_eq!(Q16::ZERO.pow(Q16::ONE), Ok(Q16::ZERO));
        assert_eq!(Q16::ZERO.pow(Q16::ZERO), Ok(Q16::ONE));
        // A negative base has no real answer, and returning zero would hide an
        // authoring mistake behind a black pixel.
        assert_eq!(Q16::from_int(-2).pow(Q16::HALF), Err(Fault::DomainError));
    }

    #[test]
    fn atan2_covers_all_four_quadrants() {
        let cases = [
            (1.0f64, 1.0f64),
            (1.0, -1.0),
            (-1.0, -1.0),
            (-1.0, 1.0),
            (0.0, 1.0),
            (1.0, 0.0),
            (0.0, -1.0),
            (-1.0, 0.0),
            (0.5, 2.0),
            (2.0, 0.5),
        ];
        for (y, x) in cases {
            let qy = Q16((y * 65536.0) as i32);
            let qx = Q16((x * 65536.0) as i32);
            close(Q16::atan2(qy, qx), y.atan2(x), 400, "atan2");
        }
        // Both zero is the one undefined input; zero is the conventional answer.
        assert_eq!(Q16::atan2(Q16::ZERO, Q16::ZERO), Q16::ZERO);
    }

    #[test]
    fn vector_lengths_are_right_and_do_not_overflow() {
        close(
            Q16::len2(Q16::from_int(3), Q16::from_int(4)),
            5.0,
            8,
            "len2 3-4-5",
        );
        close(
            Q16::len3(Q16::from_int(1), Q16::from_int(2), Q16::from_int(2)),
            3.0,
            8,
            "len3 1-2-2",
        );
        assert_eq!(Q16::len2(Q16::ZERO, Q16::ZERO), Q16::ZERO);
        // Widening matters: squaring two large values must not wrap on the way
        // to a perfectly representable answer.
        let big = Q16::from_int(300);
        close(Q16::len2(big, big), 300.0 * 2f64.sqrt(), 64, "len2 large");
    }

    #[test]
    fn isqrt_agrees_with_the_real_thing() {
        for v in [0i64, 1, 2, 3, 4, 99, 100, 10_000, 1 << 40] {
            let r = isqrt64(v);
            assert_eq!(r, (v as f64).sqrt() as i64, "isqrt({v})");
        }
        assert_eq!(isqrt64(-5), 0);
    }

    #[test]
    fn ordering_is_numeric() {
        assert!(Q16::from_int(-1) < Q16::ZERO);
        assert!(Q16::ZERO < Q16::HALF);
        assert!(Q16::HALF < Q16::ONE);
        assert!(Q16::ZERO.is_zero());
        assert!(!Q16::ONE.is_zero());
    }
}
