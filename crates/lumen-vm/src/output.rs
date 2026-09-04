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
//! steps and then stops early.
//!
//! # Dithering is available and off by default
//!
//! Trading that quantisation for temporal dithering is the obvious move and it
//! does not survive contact with a strip. A dithered pixel at the dark end
//! toggles between two codes every few frames, and a show clock runs at 30 fps —
//! so a value sitting near half a code flickers at about 15 Hz, which is close to
//! the worst frequency there is for human vision. On a bare strip a few feet
//! away it reads as the hardware malfunctioning, which is a good deal worse than
//! a fade that ends slightly early.
//!
//! Temporal dithering wants a frame rate high enough to fuse, and this project's
//! is set by the mesh's timing grid rather than by what the eye needs. So
//! [`Output::encode`] rounds by default, and [`Output::with_dither`] turns it on
//! for a device that runs faster or sits behind a diffuser. The code is kept
//! because the trade is real in those cases, and the default is what a bare
//! strip at 30 fps actually wants.
//!
//! # When it is on, the three channels of a pixel round together
//!
//! The first version diffused an error per channel, which is the textbook
//! approach and is wrong here. Red, green and blue at the dark end of a
//! fade sit at slightly different values - `#ecfbff` is (0.79, 0.96, 1.00) in
//! linear light - so their errors accumulate at different rates and each crosses
//! a code boundary on a different frame. The strip shows red, then green, then
//! blue, where it should show a dim grey. It was visible on real hardware within
//! a minute of shipping it.
//!
//! So the dither is **ordered, with one threshold per pixel per frame**, shared
//! by that pixel's three channels. They cross together, the hue holds, and what
//! is left is a dim pixel flickering rather than one changing colour.
//!
//! # And it is deterministic across devices
//!
//! The threshold comes from the frame's show time and the LED's index, both of
//! which every device in a mesh agrees on. A locally-seeded dither would make
//! two strips showing one gradient shimmer against each other, which is exactly
//! the class of disagreement the rest of this project is arranged to prevent.
//!
//! Being ordered rather than error-diffused also means there is no state to
//! carry: no residual buffer, and nothing to reset when a strip goes dark.

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
    /// Whether to dither.
    ///
    /// **Off by default**, and see the note at the top of this module: at the
    /// 30 fps a show clock runs at, a dithered pixel near half a code flickers
    /// at around 15 Hz, which looks like a fault rather than a fade. Turn it on
    /// for a device running fast enough for the toggling to fuse, or one behind
    /// a diffuser.
    pub dither: bool,
}

/// Where in the 0..1 interval this pixel's rounding threshold sits this frame.
///
/// A sixteen-frame sequence in bit-reversed order, so consecutive frames land at
/// opposite ends of the interval rather than walking across it - a value that
/// should be lit a quarter of the time is lit every fourth frame rather than four
/// frames together, which is the difference between a shimmer and a blink.
///
/// Offset by the LED's index so neighbouring pixels do not fire in unison. A
/// strip whose dark end blinks all at once is far more visible than one where
/// the same amount of light is spread along it.
fn threshold_for(led: usize, phase: u32) -> u32 {
    const PERIOD: u32 = 16;
    let k = phase.wrapping_add(led as u32 * 7) % PERIOD;
    // Bit-reversal of a four-bit counter: 0, 8, 4, 12, 2, 10, ...
    let reversed = ((k & 1) << 3) | ((k & 2) << 1) | ((k & 4) >> 1) | ((k & 8) >> 3);
    // Half a step in, so the thresholds are centred on 1/32, 3/32, ... rather
    // than starting at zero - which would round a value of exactly zero up.
    (reversed * 2 + 1) * (65_536 / (PERIOD * 2))
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
            dither: false,
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

    pub const fn with_dither(mut self, dither: bool) -> Output {
        self.dither = dither;
        self
    }

    /// Encode `linear` into `out`, carrying dither state in `residual`.
    ///
    /// All three are three entries per LED and must be the same length. A
    /// mismatch encodes what fits rather than panicking: this runs sixty times a
    /// second on a device in somebody's ceiling, and dropping a frame's tail is a
    /// better outcome than dropping the device.
    ///
    /// `phase` is the frame number, and must come from show time rather than a
    /// local counter: two devices dithering the same frame differently is
    /// exactly the disagreement this is arranged to avoid.
    pub fn encode(&self, linear: &[Q16], phase: u32, out: &mut [u8]) -> Encoded {
        let n = linear.len().min(out.len());
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
            // One threshold per pixel, shared by its three channels, so they
            // cross a code boundary on the same frame and the hue holds. Per
            // channel - which is the textbook arrangement - the three cross on
            // different frames and a dim grey comes out as red, then green,
            // then blue.
            let code = if self.dither {
                let threshold = threshold_for(i / CHANNELS, phase) as i64;
                ((target + threshold) >> 16).clamp(0, 255)
            } else {
                ((target + 32_768) >> 16).clamp(0, 255)
            };
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

    fn encode(out_stage: &Output, linear: &[Q16], phase: u32) -> ([u8; 3], Encoded) {
        let mut bytes = [0u8; 3];
        let report = out_stage.encode(linear, phase, &mut bytes);
        (bytes, report)
    }

    #[test]
    fn full_scale_is_full_scale_and_black_is_black() {
        // The two values that must be exact, on every frame of the dither
        // sequence. A white that comes out at 254 is a strip that never reaches
        // white, and a black that dithers to 1 is a room that never goes dark.
        let o = Output::new();
        for phase in 0..64 {
            assert_eq!(encode(&o, &[Q16::ONE; 3], phase).0, [255, 255, 255]);
            assert_eq!(encode(&o, &[Q16::ZERO; 3], phase).0, [0, 0, 0]);
        }
    }

    #[test]
    fn the_three_channels_of_a_pixel_cross_together() {
        // The bug this dither exists to avoid, and the reason it is ordered
        // rather than error-diffused. `#ecfbff` - a comet's tint - is three
        // slightly different values, and diffusing an error per channel made
        // them cross code boundaries on different frames: the dark end of the
        // trail flashed red, then green, then blue instead of fading grey.
        let o = Output::new().with_dither(true);
        let tint = [
            Q16((0.79 * 65536.0) as i32 / 300),
            Q16((0.96 * 65536.0) as i32 / 300),
            Q16((1.00 * 65536.0) as i32 / 300),
        ];
        for phase in 0..64 {
            let (bytes, _) = encode(&o, &tint, phase);
            // Whenever anything is lit, the ordering of the channels holds:
            // never blue alone while red is dark and green is not.
            assert!(
                bytes[0] <= bytes[1] && bytes[1] <= bytes[2],
                "phase {phase}: {bytes:?} is not in tint order"
            );
            // And they are never more than one code apart, so the hue cannot
            // swing to a primary.
            assert!(
                bytes[2] - bytes[0] <= 1,
                "phase {phase}: {bytes:?} spans more than one code"
            );
        }
    }

    #[test]
    fn a_value_below_one_code_is_reached_by_dithering() {
        // What dithering buys, for the devices that can use it. Half a code is
        // unrepresentable in eight bits and is otherwise 0 for ever, which is
        // why a fade ends slightly early.
        let o = Output::new().with_dither(true);
        let half_code = Q16(Q16::ONE.0 / 510);
        let sum: u32 = (0..32)
            .map(|phase| encode(&o, &[half_code; 3], phase).0[0] as u32)
            .sum();
        assert!((14..=18).contains(&sum), "lit {sum} codes over 32 frames");
    }

    #[test]
    fn the_average_tracks_the_value_across_the_dark_end() {
        let o = Output::new().with_dither(true);
        for numerator in 1..12u32 {
            let value = Q16((Q16::ONE.0 as i64 * numerator as i64 / 2550) as i32);
            let sum: i64 = (0..64)
                .map(|phase| encode(&o, &[value; 3], phase).0[0] as i64)
                .sum();
            let want = (value.0 as i64 * 255 * 64) >> 16;
            assert!(
                (sum - want).abs() <= 4,
                "value {numerator}/2550: summed {sum}, wanted {want}"
            );
        }
    }

    #[test]
    fn neighbouring_pixels_do_not_all_fire_on_the_same_frame() {
        // A strip whose dark end blinks in unison is far more visible than one
        // where the same light is spread along it.
        let o = Output::new().with_dither(true);
        let dim = Q16(Q16::ONE.0 / 510);
        const LEDS: usize = 10;
        let linear = [dim; LEDS * CHANNELS];
        let mut out = [0u8; LEDS * CHANNELS];
        o.encode(&linear, 0, &mut out);

        let lit = out.chunks(CHANNELS).filter(|px| px[0] > 0).count();
        assert!(
            (2..8).contains(&lit),
            "{lit} of {LEDS} pixels lit on one frame: {out:?}"
        );
        // And each lit pixel is lit on all three channels, which is the whole
        // reason the threshold is shared.
        for px in out.chunks(CHANNELS) {
            assert!(px[0] == px[1] && px[1] == px[2], "{px:?} is not grey");
        }
    }

    #[test]
    fn two_devices_dither_identically() {
        // Not a nicety. The threshold comes from show time and the LED index,
        // both of which every device agrees on; a locally-seeded dither would
        // make two strips showing one gradient shimmer against each other.
        let o = Output::new().with_dither(true);
        let value = Q16(1_000);
        for phase in 0..64 {
            assert_eq!(
                encode(&o, &[value; 3], phase).0,
                encode(&o, &[value; 3], phase).0
            );
        }
    }

    #[test]
    fn brightness_scales_the_whole_frame() {
        let o = Output::new().with_brightness(Q16::HALF);
        assert_eq!(encode(&o, &[Q16::ONE; 3], 0).0, [128, 128, 128]);
    }

    #[test]
    fn a_frame_that_fits_its_supply_is_left_alone() {
        // Thirty LEDs at full white is about 1.17 A; a 2 A supply covers it.
        let o = Output::new().with_power(PowerModel::ws2812(2_000));
        let linear = [Q16::ONE; 90];
        let mut out = [0u8; 90];
        let report = o.encode(&linear, 0, &mut out);
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
        let linear = [Q16::ONE; 180];
        let mut out = [0u8; 180];
        let report = o.encode(&linear, 0, &mut out);

        assert!(report.derated_to < Q16::ONE, "not derated");
        assert!(report.draw_ua <= 500_000, "drew {}", report.draw_ua);
        // Uniform to within the dither, so white is still white.
        let (lo, hi) = (
            *out.iter().min().expect("pixels"),
            *out.iter().max().expect("pixels"),
        );
        assert!(hi - lo <= 1, "{lo}..{hi} is not uniform");
        assert!(lo > 0 && hi < 255);
    }

    #[test]
    fn derating_keeps_the_colour_of_a_frame_that_is_not_white() {
        let o = Output::new().with_power(PowerModel::ws2812(200));
        let mut linear = [Q16::ZERO; 90];
        for led in 0..30 {
            linear[led * 3] = Q16::ONE;
            linear[led * 3 + 1] = Q16::HALF;
        }
        let mut out = [0u8; 90];
        o.encode(&linear, 0, &mut out);

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
        let mut out = [0u8; 900];
        let report = o.encode(&linear, 0, &mut out);
        assert_eq!(report.derated_to, Q16::ZERO);
        assert!(out.iter().all(|b| *b == 0));
    }

    #[test]
    fn a_mismatched_buffer_encodes_what_fits() {
        // Sixty times a second on a device nobody can reach is the wrong place
        // for a panic.
        let o = Output::new();
        let linear = [Q16::ONE; 9];
        let mut out = [0u8; 3];
        o.encode(&linear, 0, &mut out);
        assert_eq!(out, [255, 255, 255]);
    }

    #[test]
    fn values_outside_the_range_are_clamped() {
        // An effect that overshoots should be as bright as the LED goes. A
        // highlight that wrapped to black is the artefact everyone blames on the
        // strip.
        let o = Output::new();
        let (bytes, _) = encode(&o, &[Q16(Q16::ONE.0 * 4), Q16(-5_000), Q16::ONE], 0);
        assert_eq!(bytes, [255, 0, 255]);
    }

    #[test]
    fn by_default_a_value_below_one_code_is_lost() {
        // The cost of the default, stated rather than implied. It is the right
        // default anyway: at 30 fps the alternative flickers at about 15 Hz,
        // which reads as a broken strip rather than a short fade.
        let o = Output::new();
        let half_code = Q16(Q16::ONE.0 / 510);
        let mut out = [0u8; 3];
        o.encode(&[half_code; 3], 0, &mut out);
        assert_eq!(out, [0, 0, 0]);
    }

    #[test]
    fn rounding_is_to_nearest_rather_than_truncating() {
        // What the render loop used to do was `(v * 255) >> 16`, which always
        // rounds down. Half a code of bias across a frame is free to remove.
        let o = Output::new();
        let mut out = [0u8; 1];
        let value = Q16(((101i64 << 16) - 100) as i32 / 255);
        o.encode(&[value], 0, &mut out);
        assert_eq!(out[0], 101);
    }
}
