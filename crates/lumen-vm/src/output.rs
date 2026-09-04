//! The output stage: linear light in, device codes out.
//!
//! The last thing that happens to a frame, and until now the thing that did not
//! exist. `Rgb`'s documentation in `lumen-device` promised it, every board
//! definition declares a `max_current_ma` for it, and the firmware notes specify
//! how it should derate — and a rendered frame went straight to the wire as
//! `(value * 255) >> 16`.
//!
//! It lives here, in the crate both the Rust render loop and the C ABI depend
//! on, because a strip driven by C firmware and a strip driven by Rust firmware
//! must turn the same frame into the same light. Two output stages would undo
//! the fixed-point VM: bit-identical rendering that is then encoded differently
//! is not bit-identical output.
//!
//! # There is deliberately no gamma here
//!
//! It is tempting, and it would be wrong. A WS2812-class LED's PWM duty is
//! proportional to the light it emits, and a Lumen colour is already linear
//! light — `#ecfbff` is converted from sRGB at parse time precisely so that an
//! effect never handles an encoded value. Applying an sRGB curve on the way out
//! would make every strip brighter than the effect asked for, and would make
//! `0.5` mean "looks half as bright" in a system whose whole colour model says
//! it means "half the photons".
//!
//! The problem an sRGB curve is usually reached for here is real, but it is
//! **quantisation, not transfer function**: eight bits of linear PWM put half
//! the eye's usable range into the bottom fifty codes, so a fade ends in visible
//! steps and then stops early. That is what [`Output::encode`]'s dithering is
//! for, and dithering fixes it without lying about what a number means.
//!
//! # Dithering is deterministic, and has to be
//!
//! The residual is carried between frames, so the same show fed to two devices
//! produces the same codes on both. A random or time-seeded dither would make
//! two strips showing one gradient shimmer against each other, which is exactly
//! the class of disagreement the rest of this project is arranged to prevent.

use crate::q16::Q16;

/// Bytes a device consumes per LED, for a strip taking R, G and B.
pub const CHANNELS: usize = 3;

/// How much current one LED can draw, so a frame can be priced before it is
/// sent.
///
/// Numbers are per output, not per chip family: a 5 V strip and a 12 V strip
/// with the same part draw differently, and the board definition is the only
/// place that knows which is on the end of the wire.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PowerModel {
    /// Milliamps one colour channel draws at full, times 1000.
    ///
    /// Scaled by a thousand because a WS2812 channel is about 12.8 mA and
    /// rounding that to 12 or 13 is a 6% error in the budget — which is the
    /// difference between derating a frame that did not need it and browning out
    /// a board.
    pub channel_ua: u32,
    /// Microamps an LED draws doing nothing. Small individually and not at all
    /// small over three hundred of them.
    pub idle_ua: u32,
    /// What the supply can give, in milliamps.
    pub budget_ma: u32,
}

impl PowerModel {
    /// A 5 V WS2812/SK6812 strip: about 12.8 mA per channel, 0.7 mA idle.
    ///
    /// Datasheet-typical rather than measured, and deliberately on the generous
    /// side: a model that under-predicts derates too little and browns out,
    /// which is the failure this exists to prevent.
    pub const fn ws2812(budget_ma: u32) -> PowerModel {
        PowerModel {
            channel_ua: 12_800,
            idle_ua: 700,
            budget_ma,
        }
    }
}

/// Turns a rendered frame into the bytes a strip consumes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Output {
    /// Global brightness, 0..1. What a dimmer or a night mode moves.
    pub brightness: Q16,
    /// The supply, if this device knows about one.
    pub power: Option<PowerModel>,
}

impl Default for Output {
    fn default() -> Output {
        Output::new()
    }
}

/// What one frame cost, so a device can say why it is dim.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Encoded {
    /// Microamps the frame is predicted to draw after derating.
    pub draw_ua: u32,
    /// The factor the frame was scaled by, `Q16::ONE` when it fitted.
    ///
    /// Reported rather than silent: a strip that is quietly at 60% because the
    /// supply is too small looks like an effect that is quietly wrong, and the
    /// two are debugged in completely different places.
    pub derated_to: Q16,
}

impl Output {
    pub const fn new() -> Output {
        Output {
            brightness: Q16::ONE,
            power: None,
        }
    }

    pub const fn with_brightness(mut self, brightness: Q16) -> Output {
        self.brightness = brightness;
        self
    }

    pub const fn with_power(mut self, power: PowerModel) -> Output {
        self.power = Some(power);
        self
    }

    /// Encode `linear` into `out`, carrying dither state in `residual`.
    ///
    /// All three are three entries per LED and must be the same length. A
    /// mismatch encodes what fits rather than panicking: this runs sixty times a
    /// second on a device in somebody's ceiling, and dropping a frame's tail is a
    /// better outcome than dropping the device.
    ///
    /// `residual` must persist between frames and start at zero. Reset it when
    /// the strip goes dark, or the first frame after a blackout carries a
    /// fraction of the last one.
    ///
    /// `None` turns dithering off and rounds to nearest instead. Everything
    /// else (brightness, the power budget) still applies, so the two paths
    /// differ only in what they do with the part of a value below one code.
    /// Turning it off costs the bottom of every fade, and is there for a device
    /// with no room for the state.
    pub fn encode(
        &self,
        linear: &[Q16],
        mut residual: Option<&mut [i32]>,
        out: &mut [u8],
    ) -> Encoded {
        let n = match &residual {
            Some(r) => linear.len().min(r.len()).min(out.len()),
            None => linear.len().min(out.len()),
        };
        let leds = n / CHANNELS;

        // Brightness first, then the supply. Both are global scalings and they
        // compose, but the order matters for the report: a frame dimmed by a
        // user is not a frame that was too big for its supply, and only the
        // second is worth telling anyone about.
        let mut scale = self.brightness.0.clamp(0, Q16::ONE.0) as i64;

        let mut derated_to = Q16::ONE;
        if let Some(power) = self.power {
            // Priced after the brightness scaling, because that is the frame
            // that will actually be sent.
            let mut scaled_draw = 0u32;
            for v in &linear[..n] {
                let clamped = v.0.clamp(0, Q16::ONE.0) as i64;
                let after = ((clamped * scale) >> 16) as u64;
                let part = (after * power.channel_ua as u64) >> 16;
                scaled_draw = scaled_draw.saturating_add(part as u32);
            }
            scaled_draw = scaled_draw.saturating_add((leds as u32).saturating_mul(power.idle_ua));

            let budget_ua = power.budget_ma.saturating_mul(1_000);
            if scaled_draw > budget_ua && scaled_draw > 0 {
                // Scale the whole frame, never clip a pixel. An over-budget
                // frame should dim uniformly; clipping the brightest pixels
                // changes the colours of exactly the parts of the picture
                // someone is looking at.
                //
                // The idle draw is not scalable - an LED that is on at all pays
                // it - so the budget available to the colour channels is what is
                // left after it. A strip whose idle draw alone exceeds the
                // supply is a hardware problem and derates to zero here rather
                // than pretending.
                let idle = (leds as u32).saturating_mul(power.idle_ua);
                // Reserve what the quantiser can round up by. `encode` rounds
                // each channel to the nearest code, so an individual frame can
                // exceed its target by half a code per channel even though the
                // dithered average is exact - and a supply has to survive the
                // peak, not the average. Half a code is `channel_ua / 510`.
                let rounding = ((n as u64 * power.channel_ua as u64) / 510) as u32;
                let colour_budget = budget_ua.saturating_sub(idle).saturating_sub(rounding);
                let colour_draw = scaled_draw.saturating_sub(idle).max(1);
                let factor = ((colour_budget as u64) << 16) / colour_draw as u64;
                derated_to = Q16(factor.min(Q16::ONE.0 as u64) as i32);
                scale = (scale * derated_to.0 as i64) >> 16;
            }
        }

        let mut total_ua = 0u32;
        for i in 0..n {
            let clamped = linear[i].0.clamp(0, Q16::ONE.0) as i64;
            let scaled = (clamped * scale) >> 16;

            // The value we want, in codes, with sixteen fractional bits to
            // spare. `255` rather than `256`: full scale must land exactly on
            // 255 rather than one short of it.
            let target = scaled * 255;
            let carried = residual.as_ref().map_or(0, |r| r[i]) as i64;
            let acc = target + carried;
            // Round to nearest, then keep what was thrown away. Over a few
            // frames the average code equals the value asked for, which is how
            // eight bits of linear PWM manages a fade that ends smoothly
            // instead of in four visible steps.
            let code = ((acc + 32_768) >> 16).clamp(0, 255);
            if let Some(r) = residual.as_mut() {
                r[i] = (acc - (code << 16)).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            }
            out[i] = code as u8;

            if let Some(power) = self.power {
                total_ua =
                    total_ua.saturating_add(((code as u64 * power.channel_ua as u64) / 255) as u32);
            }
        }
        if let Some(power) = self.power {
            total_ua = total_ua.saturating_add((leds as u32).saturating_mul(power.idle_ua));
        }

        Encoded {
            // Priced from the codes actually written rather than from the
            // values asked for, so the number reported is the current the strip
            // will really draw - dithering and rounding included.
            draw_ua: total_ua,
            derated_to,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(out_stage: &Output, linear: &[Q16], residual: &mut [i32]) -> ([u8; 3], Encoded) {
        let mut bytes = [0u8; 3];
        let report = out_stage.encode(linear, Some(residual), &mut bytes);
        (bytes, report)
    }

    #[test]
    fn full_scale_is_full_scale_and_black_is_black() {
        // The two values that must be exact. A white that comes out at 254 is a
        // strip that never reaches white, and a black that dithers to 1 is a
        // room that never goes dark.
        let o = Output::new();
        let mut residual = [0i32; 3];
        let (bytes, _) = encode(&o, &[Q16::ONE; 3], &mut residual);
        assert_eq!(bytes, [255, 255, 255]);

        let mut residual = [0i32; 3];
        for _ in 0..100 {
            let (bytes, _) = encode(&o, &[Q16::ZERO; 3], &mut residual);
            assert_eq!(bytes, [0, 0, 0], "black dithered");
        }
    }

    #[test]
    fn a_value_below_one_code_is_reached_by_dithering() {
        // The whole point. Half a code is unrepresentable in eight bits, and
        // without dithering it is either 0 or 1 for ever - which is why the dark
        // end of a fade ends in steps and then stops early.
        let o = Output::new();
        let half_code = Q16(Q16::ONE.0 / 510); // half of 1/255
        let mut residual = [0i32; 3];
        let mut sum = 0u32;
        const FRAMES: u32 = 100;
        for _ in 0..FRAMES {
            let (bytes, _) = encode(&o, &[half_code; 3], &mut residual);
            sum += bytes[0] as u32;
        }
        // Half a code over a hundred frames is fifty codes' worth of light.
        assert!((45..=55).contains(&sum), "got {sum}");
    }

    #[test]
    fn the_average_tracks_the_value_across_the_dark_end() {
        let o = Output::new();
        for numerator in 1..12u32 {
            let value = Q16((Q16::ONE.0 as i64 * numerator as i64 / 2550) as i32);
            let mut residual = [0i32; 3];
            let mut sum = 0i64;
            const FRAMES: i64 = 200;
            for _ in 0..FRAMES {
                let (bytes, _) = encode(&o, &[value; 3], &mut residual);
                sum += bytes[0] as i64;
            }
            let want = (value.0 as i64 * 255 * FRAMES) >> 16;
            assert!(
                (sum - want).abs() <= 2,
                "value {numerator}/2550: summed {sum}, wanted {want}"
            );
        }
    }

    #[test]
    fn two_devices_dither_identically() {
        // Not a nicety. A random or time-seeded dither would make two strips
        // showing one gradient shimmer against each other, which is the class of
        // disagreement everything else in this project is arranged to prevent.
        let o = Output::new();
        let value = Q16(1_000);
        let (mut a, mut b) = ([0i32; 3], [0i32; 3]);
        for _ in 0..64 {
            let (x, _) = encode(&o, &[value; 3], &mut a);
            let (y, _) = encode(&o, &[value; 3], &mut b);
            assert_eq!(x, y);
        }
    }

    #[test]
    fn brightness_scales_the_whole_frame() {
        let o = Output::new().with_brightness(Q16::HALF);
        let mut residual = [0i32; 3];
        let (bytes, _) = encode(&o, &[Q16::ONE; 3], &mut residual);
        assert_eq!(bytes, [128, 128, 128]);
    }

    #[test]
    fn a_frame_that_fits_its_supply_is_left_alone() {
        // Thirty LEDs at full white is about 1.17 A; a 2 A supply covers it.
        let o = Output::new().with_power(PowerModel::ws2812(2_000));
        let linear = [Q16::ONE; 90];
        let mut residual = [0i32; 90];
        let mut out = [0u8; 90];
        let report = o.encode(&linear, Some(&mut residual), &mut out);
        assert_eq!(report.derated_to, Q16::ONE);
        assert!(out.iter().all(|b| *b == 255));
        assert!(report.draw_ua < 2_000_000, "{}", report.draw_ua);
    }

    #[test]
    fn an_over_budget_frame_dims_uniformly_rather_than_clipping() {
        // The specified behaviour: an over-budget frame dims all over rather
        // than losing its highlights, because clipping changes the colour of
        // exactly the parts somebody is looking at.
        let o = Output::new().with_power(PowerModel::ws2812(500));
        // Sixty LEDs of full white: about 2.3 A against a 500 mA supply.
        let linear = [Q16::ONE; 180];
        let mut residual = [0i32; 180];
        let mut out = [0u8; 180];
        let report = o.encode(&linear, Some(&mut residual), &mut out);

        assert!(report.derated_to < Q16::ONE, "not derated");
        assert!(report.draw_ua <= 500_000, "drew {}", report.draw_ua);
        // Uniform: every channel took the same scaling, so white is still white.
        let first = out[0];
        assert!(out.iter().all(|b| *b == first), "not uniform");
        assert!(first > 0 && first < 255, "{first}");
    }

    #[test]
    fn derating_keeps_the_colour_of_a_frame_that_is_not_white() {
        let o = Output::new().with_power(PowerModel::ws2812(200));
        let mut linear = [Q16::ZERO; 90];
        for led in 0..30 {
            linear[led * 3] = Q16::ONE;
            linear[led * 3 + 1] = Q16::HALF;
        }
        let mut residual = [0i32; 90];
        let mut out = [0u8; 90];
        o.encode(&linear, Some(&mut residual), &mut out);

        // Red is still twice green, and blue is still off.
        for led in 0..30 {
            let (r, g, b) = (out[led * 3], out[led * 3 + 1], out[led * 3 + 2]);
            assert_eq!(b, 0);
            assert!(
                (r as i32 - 2 * g as i32).abs() <= 2,
                "led {led}: {r} vs {g}"
            );
        }
    }

    #[test]
    fn a_supply_smaller_than_the_idle_draw_derates_to_nothing() {
        // Three hundred LEDs idle at about 210 mA before anything is lit. A
        // 100 mA supply cannot run this strip at all, and saying so with a black
        // frame is better than browning out a board halfway through one.
        let o = Output::new().with_power(PowerModel::ws2812(100));
        let linear = [Q16::ONE; 900];
        let mut residual = [0i32; 900];
        let mut out = [0u8; 900];
        let report = o.encode(&linear, Some(&mut residual), &mut out);
        assert_eq!(report.derated_to, Q16::ZERO);
        assert!(out.iter().all(|b| *b == 0));
    }

    #[test]
    fn a_mismatched_buffer_encodes_what_fits() {
        // Sixty times a second on a device nobody can reach is the wrong place
        // for a panic.
        let o = Output::new();
        let linear = [Q16::ONE; 9];
        let mut residual = [0i32; 9];
        let mut out = [0u8; 3];
        o.encode(&linear, Some(&mut residual), &mut out);
        assert_eq!(out, [255, 255, 255]);
    }

    #[test]
    fn values_outside_the_range_are_clamped() {
        // An effect that overshoots should be as bright as the LED goes. A
        // highlight that wrapped to black is the artefact everyone blames on the
        // strip.
        let o = Output::new();
        let mut residual = [0i32; 3];
        let (bytes, _) = encode(
            &o,
            &[Q16(Q16::ONE.0 * 4), Q16(-5_000), Q16::ONE],
            &mut residual,
        );
        assert_eq!(bytes, [255, 0, 255]);
    }
}

#[cfg(test)]
mod undithered_tests {
    use super::*;

    #[test]
    fn without_dither_a_value_below_one_code_is_lost() {
        // The cost of turning it off, stated rather than implied. Half a code
        // rounds to zero every frame and stays there, which is the bottom of
        // every fade going missing.
        let o = Output::new();
        let half_code = Q16(Q16::ONE.0 / 510);
        let mut out = [0u8; 3];
        o.encode(&[half_code; 3], None, &mut out);
        assert_eq!(out, [0, 0, 0]);

        // With it, the same value reaches the strip half the time.
        let mut residual = [0i32; 3];
        let lit = (0..10)
            .filter(|_| {
                o.encode(&[half_code; 3], Some(&mut residual), &mut out);
                out[0] > 0
            })
            .count();
        assert!((4..=6).contains(&lit), "lit on {lit} frames of ten");
    }

    #[test]
    fn brightness_and_derating_still_apply_without_dither() {
        // Only the sub-code part changes. A device with no room for the state
        // must still not brown out its supply.
        let o = Output::new().with_power(PowerModel::ws2812(500));
        let linear = [Q16::ONE; 180];
        let mut out = [0u8; 180];
        let report = o.encode(&linear, None, &mut out);
        assert!(report.derated_to < Q16::ONE);
        assert!(report.draw_ua <= 500_000, "drew {}", report.draw_ua);
        assert!(out.iter().all(|b| *b == out[0]));
    }

    #[test]
    fn rounding_is_to_nearest_rather_than_truncating() {
        // What the render loop used to do was `(v * 255) >> 16`, which always
        // rounds down. Half a code of bias across a whole frame is free to
        // remove and worth removing.
        let o = Output::new();
        let mut out = [0u8; 1];
        // 0.999 of a code above 100: truncation gives 100, nearest gives 101.
        let value = Q16(((101i64 << 16) - 100) as i32 / 255);
        o.encode(&[value], None, &mut out);
        assert_eq!(out[0], 101);
    }
}
