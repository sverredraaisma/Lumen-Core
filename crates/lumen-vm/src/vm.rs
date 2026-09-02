//! The interpreter.
//!
//! # The register file persists across sections
//!
//! `frame` runs once, then `pixel` runs once per LED **on the same machine**,
//! and only the input registers are rewritten between pixels. So a value the
//! compiler hoists out of `pixel` into `frame` is computed once and simply read
//! three hundred times.
//!
//! That is the entire performance story of this design, and it is why hoisting
//! is worth the compiler complexity: registers [`R_SCRATCH`] and above survive
//! from `frame` into every pixel of that frame.
//!
//! # Budget
//!
//! Every instruction is charged. The budget is a **backstop**, not the primary
//! defence — the compiler is supposed to prove the cost before publishing, and
//! reaching [`Fault::BudgetExceeded`] means the estimate was wrong. Having it
//! anyway is what makes "a malicious program can waste its own cycles and
//! nothing else" true rather than aspirational.

use crate::isa::{check_reg, check_reg_run, Instruction, OpCode, REG_COUNT};
use crate::noise::{noise1, noise2, noise3};
use crate::program::{Program, Section};
use crate::q16::Q16;
use crate::Fault;

/// LED world coordinate, x.
pub const R_X: u8 = 0;
pub const R_Y: u8 = 1;
pub const R_Z: u8 = 2;
/// Coordinates local to the device root.
pub const R_LX: u8 = 3;
pub const R_LY: u8 = 4;
pub const R_LZ: u8 = 5;
/// LED index within the device.
pub const R_I: u8 = 6;
/// LED count on the device.
pub const R_N: u8 = 7;
/// Normalised 0..1 along the **zone projection of the source being rendered**,
/// not along the strip.
pub const R_U: u8 = 8;
pub const R_UV_X: u8 = 9;
pub const R_UV_Y: u8 = 10;
/// Show time in seconds.
pub const R_T: u8 = 11;
/// This pixel's value last frame — the local history buffer.
pub const R_PREV: u8 = 12;

/// First register not overwritten per pixel.
///
/// Everything from here up survives from `frame` into every pixel, which is what
/// makes hoisting pay.
pub const R_SCRATCH: u8 = 13;

/// How deep `REPEAT` blocks and `CALL`s may nest.
///
/// Small and fixed: the register file is 32 words, so a program needing deeper
/// nesting than this is doing something the compiler should have unrolled.
pub const STACK_DEPTH: usize = 8;

/// Per-pixel inputs.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct PixelInputs {
    pub x: Q16,
    pub y: Q16,
    pub z: Q16,
    pub lx: Q16,
    pub ly: Q16,
    pub lz: Q16,
    pub index: Q16,
    pub count: Q16,
    /// Position along the source's zone projection.
    ///
    /// Not a property of the pixel: an LED covered by three overlapping sources
    /// has three different values of `u` in one frame, because each source
    /// projects it differently. Never cache it per pixel.
    pub u: Q16,
    pub uv_x: Q16,
    pub uv_y: Q16,
    /// This pixel's value last frame.
    pub prev: Q16,
}

/// What a `pixel` section emitted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixelOutput {
    /// The program ran to completion without emitting.
    ///
    /// Legal and useful: a masked-off pixel emits nothing, and the compositor
    /// leaves whatever was underneath.
    None,
    Rgb {
        r: Q16,
        g: Q16,
        b: Q16,
    },
    Rgbw {
        r: Q16,
        g: Q16,
        b: Q16,
        w: Q16,
    },
    /// Correlated colour temperature in kelvin, plus intensity.
    Cct {
        kelvin: Q16,
        intensity: Q16,
    },
}

/// Everything the program reads from outside itself.
///
/// A trait rather than a struct so the simulator, the firmware and a test can
/// each supply channels their own way — and so a test needs no network to run a
/// program that reads one.
pub trait Uniforms {
    /// Read offset `offset` of the channel bound to `slot`.
    ///
    /// A slot with no channel, or a stale one, returns its default rather than
    /// failing: **defined degradation**. A dead audio publisher must leave the
    /// lights doing something sensible, not stop the program.
    fn channel(&self, slot: u8, offset: u8) -> Q16;

    /// Record a probe value. Only ever called by a probe build.
    fn probe(&mut self, _probe_id: u16, _value: Q16) {}
}

/// A [`Uniforms`] where every channel reads zero.
///
/// The default a device falls back to when nothing is publishing.
#[derive(Clone, Copy, Default, Debug)]
pub struct NoUniforms;

impl Uniforms for NoUniforms {
    fn channel(&self, _slot: u8, _offset: u8) -> Q16 {
        Q16::ZERO
    }
}

/// The register machine.
#[derive(Clone, Debug)]
pub struct Machine {
    regs: [Q16; REG_COUNT],
    /// `(loop start index, iterations remaining)`
    repeat: [(usize, u16); STACK_DEPTH],
    repeat_depth: usize,
    calls: [usize; STACK_DEPTH],
    call_depth: usize,
    /// Value written by `PREVWRITE`, to be fed back as `prev` next frame.
    prev_out: Q16,
    spent: u32,
    limit: u32,
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    pub const fn new() -> Machine {
        Machine {
            regs: [Q16::ZERO; REG_COUNT],
            repeat: [(0, 0); STACK_DEPTH],
            repeat_depth: 0,
            calls: [0; STACK_DEPTH],
            call_depth: 0,
            prev_out: Q16::ZERO,
            spent: 0,
            limit: u32::MAX,
        }
    }

    /// Cap on budget units per section invocation.
    pub fn set_budget(&mut self, limit: u32) {
        self.limit = limit;
    }

    /// Budget spent by the last invocation.
    pub fn spent(&self) -> u32 {
        self.spent
    }

    pub fn register(&self, r: u8) -> Option<Q16> {
        self.regs.get(r as usize).copied()
    }

    pub fn set_register(&mut self, r: u8, v: Q16) -> Result<(), Fault> {
        let i = check_reg(r)?;
        self.regs[i] = v;
        Ok(())
    }

    /// The value `PREVWRITE` left, to be fed back as `prev` next frame.
    pub fn prev_out(&self) -> Q16 {
        self.prev_out
    }

    /// Clear the register file. Not done between pixels — that is the point.
    pub fn reset(&mut self) {
        self.regs = [Q16::ZERO; REG_COUNT];
        self.repeat_depth = 0;
        self.call_depth = 0;
        self.prev_out = Q16::ZERO;
    }

    fn load_inputs(&mut self, inp: &PixelInputs) {
        self.regs[R_X as usize] = inp.x;
        self.regs[R_Y as usize] = inp.y;
        self.regs[R_Z as usize] = inp.z;
        self.regs[R_LX as usize] = inp.lx;
        self.regs[R_LY as usize] = inp.ly;
        self.regs[R_LZ as usize] = inp.lz;
        self.regs[R_I as usize] = inp.index;
        self.regs[R_N as usize] = inp.count;
        self.regs[R_U as usize] = inp.u;
        self.regs[R_UV_X as usize] = inp.uv_x;
        self.regs[R_UV_Y as usize] = inp.uv_y;
        self.regs[R_PREV as usize] = inp.prev;
        self.prev_out = inp.prev;
    }

    /// Run the `once` section. Call on activation.
    pub fn run_once<U: Uniforms>(
        &mut self,
        program: &Program<'_>,
        uniforms: &mut U,
    ) -> Result<(), Fault> {
        self.execute(program, Section::Once, uniforms).map(|_| ())
    }

    /// Run the `frame` section. Call once per frame, before any pixel.
    ///
    /// Set `t` first with [`Machine::set_register`] on [`R_T`], or through
    /// [`Machine::run_frame_at`].
    pub fn run_frame<U: Uniforms>(
        &mut self,
        program: &Program<'_>,
        uniforms: &mut U,
    ) -> Result<(), Fault> {
        self.execute(program, Section::Frame, uniforms).map(|_| ())
    }

    /// Run the `frame` section at show time `t`, in seconds.
    pub fn run_frame_at<U: Uniforms>(
        &mut self,
        program: &Program<'_>,
        t: Q16,
        uniforms: &mut U,
    ) -> Result<(), Fault> {
        self.regs[R_T as usize] = t;
        self.run_frame(program, uniforms)
    }

    /// Run the `pixel` section for one LED.
    ///
    /// Registers below [`R_SCRATCH`] are overwritten from `inputs`; everything
    /// above survives from `frame`.
    pub fn run_pixel<U: Uniforms>(
        &mut self,
        program: &Program<'_>,
        inputs: &PixelInputs,
        uniforms: &mut U,
    ) -> Result<PixelOutput, Fault> {
        self.load_inputs(inputs);
        self.execute(program, Section::Pixel, uniforms)
    }

    fn charge(&mut self, cost: u32) -> Result<(), Fault> {
        self.spent = self.spent.saturating_add(cost);
        if self.spent > self.limit {
            return Err(Fault::BudgetExceeded);
        }
        Ok(())
    }

    fn execute<U: Uniforms>(
        &mut self,
        program: &Program<'_>,
        section: Section,
        uniforms: &mut U,
    ) -> Result<PixelOutput, Fault> {
        self.spent = 0;
        self.repeat_depth = 0;
        self.call_depth = 0;
        let count = program.section_len(section);
        let mut pc = 0usize;
        let mut out = PixelOutput::None;

        while pc < count {
            let ins = program.instruction(section, pc).ok_or(Fault::BadProgram)?;
            self.charge(ins.op.cost())?;
            pc += 1;
            match self.step(program, section, ins, uniforms, &mut pc, &mut out)? {
                Flow::Continue => {}
                Flow::Halt => break,
            }
        }
        Ok(out)
    }

    fn reg(&self, r: u8) -> Result<Q16, Fault> {
        Ok(self.regs[check_reg(r)?])
    }

    fn put(&mut self, r: u8, v: Q16) -> Result<(), Fault> {
        let i = check_reg(r)?;
        self.regs[i] = v;
        Ok(())
    }

    /// Read `n` consecutive registers starting at `r`.
    fn reg_run<const N: usize>(&self, r: u8) -> Result<[Q16; N], Fault> {
        let base = check_reg_run(r, N as u8)?;
        let mut out = [Q16::ZERO; N];
        for (k, slot) in out.iter_mut().enumerate() {
            *slot = self.regs[base + k];
        }
        Ok(out)
    }

    fn put_run<const N: usize>(&mut self, r: u8, vals: [Q16; N]) -> Result<(), Fault> {
        let base = check_reg_run(r, N as u8)?;
        for (k, v) in vals.into_iter().enumerate() {
            self.regs[base + k] = v;
        }
        Ok(())
    }

    fn step<U: Uniforms>(
        &mut self,
        program: &Program<'_>,
        section: Section,
        ins: Instruction,
        uniforms: &mut U,
        pc: &mut usize,
        out: &mut PixelOutput,
    ) -> Result<Flow, Fault> {
        use OpCode::*;
        let (a, b, c) = (ins.a, ins.b, ins.c);
        match ins.op {
            Nop => {}
            Mov => {
                let v = self.reg(b)?;
                self.put(a, v)?;
            }
            LoadK => {
                let v = program.constant(ins.bc()).ok_or(Fault::BadProgram)?;
                self.put(a, v)?;
            }
            Add => {
                let v = self.reg(b)?.add(self.reg(c)?);
                self.put(a, v)?;
            }
            Sub => {
                let v = self.reg(b)?.sub(self.reg(c)?);
                self.put(a, v)?;
            }
            Mul => {
                let v = self.reg(b)?.mul(self.reg(c)?);
                self.put(a, v)?;
            }
            Div => {
                let v = self.reg(b)?.div(self.reg(c)?)?;
                self.put(a, v)?;
            }
            Madd => {
                let v = self.reg(a)?.madd(self.reg(b)?, self.reg(c)?);
                self.put(a, v)?;
            }
            Neg => {
                let v = self.reg(b)?.neg();
                self.put(a, v)?;
            }
            Abs => {
                let v = self.reg(b)?.abs();
                self.put(a, v)?;
            }
            Min => {
                let v = self.reg(b)?.min(self.reg(c)?);
                self.put(a, v)?;
            }
            Max => {
                let v = self.reg(b)?.max(self.reg(c)?);
                self.put(a, v)?;
            }
            Clamp => {
                let v = self.reg(a)?.clamp(self.reg(b)?, self.reg(c)?);
                self.put(a, v)?;
            }
            Floor => {
                let v = self.reg(b)?.floor();
                self.put(a, v)?;
            }
            Fract => {
                let v = self.reg(b)?.fract();
                self.put(a, v)?;
            }
            Sin => {
                let v = self.reg(b)?.sin();
                self.put(a, v)?;
            }
            Cos => {
                let v = self.reg(b)?.cos();
                self.put(a, v)?;
            }
            SinTurns => {
                let v = self.reg(b)?.sin_turns();
                self.put(a, v)?;
            }
            CosTurns => {
                let v = self.reg(b)?.cos_turns();
                self.put(a, v)?;
            }
            Atan2 => {
                let v = Q16::atan2(self.reg(b)?, self.reg(c)?);
                self.put(a, v)?;
            }
            Sqrt => {
                let v = self.reg(b)?.sqrt()?;
                self.put(a, v)?;
            }
            Pow => {
                let v = self.reg(b)?.pow(self.reg(c)?)?;
                self.put(a, v)?;
            }
            Exp => {
                let v = self.reg(b)?.exp();
                self.put(a, v)?;
            }
            Log => {
                let v = self.reg(b)?.ln()?;
                self.put(a, v)?;
            }
            Log2 => {
                let v = self.reg(b)?.log2()?;
                self.put(a, v)?;
            }
            Noise1 => {
                let v = noise1(self.reg(b)?);
                self.put(a, v)?;
            }
            Noise2 => {
                let v = noise2(self.reg(b)?, self.reg(c)?);
                self.put(a, v)?;
            }
            Noise3 => {
                let [x, y, z] = self.reg_run::<3>(b)?;
                self.put(a, noise3(x, y, z))?;
            }
            Lt => self.compare(a, b, c, |x, y| x < y)?,
            Le => self.compare(a, b, c, |x, y| x <= y)?,
            Gt => self.compare(a, b, c, |x, y| x > y)?,
            Ge => self.compare(a, b, c, |x, y| x >= y)?,
            Eq => self.compare(a, b, c, |x, y| x == y)?,
            Select => {
                // Branchless on purpose: a per-pixel branch on data costs more
                // than evaluating both sides of a two-register choice.
                let v = if self.reg(a)?.is_zero() {
                    self.reg(c)?
                } else {
                    self.reg(b)?
                };
                self.put(a, v)?;
            }
            Step => {
                let v = self.reg(b)?.step(self.reg(c)?);
                self.put(a, v)?;
            }
            SmoothStep => {
                let v = self.reg(a)?.smoothstep(self.reg(b)?, self.reg(c)?);
                self.put(a, v)?;
            }
            Lerp => {
                let v = self.reg(a)?.lerp(self.reg(b)?, self.reg(c)?);
                self.put(a, v)?;
            }
            Len2 => {
                let v = Q16::len2(self.reg(b)?, self.reg(c)?);
                self.put(a, v)?;
            }
            Len3 => {
                let [x, y, z] = self.reg_run::<3>(b)?;
                self.put(a, Q16::len3(x, y, z))?;
            }
            Dot3 => {
                let [x0, y0, z0] = self.reg_run::<3>(b)?;
                let [x1, y1, z1] = self.reg_run::<3>(c)?;
                let v = x0.mul(x1).add(y0.mul(y1)).add(z0.mul(z1));
                self.put(a, v)?;
            }
            Hsv2Rgb => {
                let [h, s, v] = self.reg_run::<3>(b)?;
                self.put_run::<3>(a, hsv_to_rgb(h, s, v))?;
            }
            Rgb2Hsv => {
                let [r, g, bl] = self.reg_run::<3>(b)?;
                self.put_run::<3>(a, rgb_to_hsv(r, g, bl))?;
            }
            Palette => {
                let pos = self.reg(b)?;
                let (r, g, bl) = program.palette_sample(c, pos).ok_or(Fault::BadProgram)?;
                self.put_run::<3>(a, [r, g, bl])?;
            }
            Temp2Rgb => {
                let k = self.reg(b)?;
                self.put_run::<3>(a, kelvin_to_rgb(k))?;
            }
            ChRead => {
                let v = uniforms.channel(b, c);
                self.put(a, v)?;
            }
            PrevRead => {
                let v = self.regs[R_PREV as usize];
                self.put(a, v)?;
            }
            PrevWrite => {
                self.prev_out = self.reg(a)?;
            }
            MaskTest => {
                // The early-out that makes layered effects affordable: a
                // masked-off pixel costs a handful of instructions instead of a
                // whole layer stack.
                if self.reg(a)?.is_zero() {
                    *pc += ins.bc() as usize;
                }
            }
            Repeat => {
                if self.repeat_depth >= STACK_DEPTH {
                    return Err(Fault::StackOverflow);
                }
                let trips = ins.bc();
                if trips == 0 {
                    // Skip to the matching ENDREP rather than executing the body
                    // once, which is what a naive implementation does.
                    *pc = self.skip_to_endrep(program, section, *pc)?;
                } else {
                    self.repeat[self.repeat_depth] = (*pc, trips);
                    self.repeat_depth += 1;
                }
            }
            EndRep => {
                if self.repeat_depth == 0 {
                    return Err(Fault::BadProgram);
                }
                let top = self.repeat_depth - 1;
                let (start, left) = self.repeat[top];
                if left > 1 {
                    self.repeat[top] = (start, left - 1);
                    *pc = start;
                } else {
                    self.repeat_depth -= 1;
                }
            }
            Call => {
                if self.call_depth >= STACK_DEPTH {
                    return Err(Fault::StackOverflow);
                }
                self.calls[self.call_depth] = *pc;
                self.call_depth += 1;
                *pc = ins.bc() as usize;
            }
            Ret => {
                if self.call_depth == 0 {
                    return Ok(Flow::Halt);
                }
                self.call_depth -= 1;
                *pc = self.calls[self.call_depth];
            }
            Halt => return Ok(Flow::Halt),
            ALoad | AStore | ALen => {
                // Structurally excluded from `pixel` programs at load. A `sim`
                // machine with an array bank implements these; reaching here
                // means one was run without one.
                return Err(Fault::UnsupportedInstruction(ins.op.to_u8()));
            }
            Probe => {
                let v = self.reg(a)?;
                uniforms.probe(ins.bc(), v);
            }
            EmitRgb => {
                *out = PixelOutput::Rgb {
                    r: self.reg(a)?,
                    g: self.reg(b)?,
                    b: self.reg(c)?,
                };
            }
            EmitRgbw => {
                let [r, g, b2, w] = self.reg_run::<4>(a)?;
                *out = PixelOutput::Rgbw { r, g, b: b2, w };
            }
            EmitCct => {
                *out = PixelOutput::Cct {
                    kelvin: self.reg(a)?,
                    intensity: self.reg(b)?,
                };
            }
        }
        Ok(Flow::Continue)
    }

    fn compare(&mut self, a: u8, b: u8, c: u8, f: fn(Q16, Q16) -> bool) -> Result<(), Fault> {
        let v = if f(self.reg(b)?, self.reg(c)?) {
            Q16::ONE
        } else {
            Q16::ZERO
        };
        self.put(a, v)
    }

    /// Find the instruction after the `ENDREP` matching a `REPEAT` at `from`.
    ///
    /// Only reached by a zero-trip `REPEAT`, which must skip its body rather
    /// than run it once - the mistake a naive implementation makes.
    fn skip_to_endrep(
        &self,
        program: &Program<'_>,
        section: Section,
        from: usize,
    ) -> Result<usize, Fault> {
        let count = program.section_len(section);
        let mut depth = 1;
        let mut i = from;
        while i < count {
            let ins = program.instruction(section, i).ok_or(Fault::BadProgram)?;
            match ins.op {
                OpCode::Repeat => depth += 1,
                OpCode::EndRep => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(i + 1);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        Err(Fault::BadProgram)
    }
}

enum Flow {
    Continue,
    Halt,
}

/// HSV to RGB, all channels in 0..1 with hue in turns.
///
/// Hue in turns rather than degrees so `hue = t` is one rotation per second with
/// no scaling constant anywhere.
pub fn hsv_to_rgb(h: Q16, s: Q16, v: Q16) -> [Q16; 3] {
    let s = s.clamp(Q16::ZERO, Q16::ONE);
    let v = v.clamp(Q16::ZERO, Q16::ONE);
    let h6 = h.fract().mul(Q16::from_int(6));
    let sector = h6.to_int();
    let f = h6.fract();
    let p = v.mul(Q16::ONE.sub(s));
    let q = v.mul(Q16::ONE.sub(s.mul(f)));
    let t = v.mul(Q16::ONE.sub(s.mul(Q16::ONE.sub(f))));
    match sector {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

/// RGB to HSV, all channels in 0..1 with hue in turns.
pub fn rgb_to_hsv(r: Q16, g: Q16, b: Q16) -> [Q16; 3] {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max.sub(min);
    if delta.is_zero() || max.is_zero() {
        return [Q16::ZERO, Q16::ZERO, max];
    }
    let sixth = Q16::from_ratio(1, 6);
    // delta is non-zero, so none of these divisions can fault.
    let scaled = |n: Q16| n.div(delta).unwrap_or(Q16::ZERO).mul(sixth);
    let h = if max == r {
        scaled(g.sub(b))
    } else if max == g {
        scaled(b.sub(r)).add(Q16::from_ratio(1, 3))
    } else {
        scaled(r.sub(g)).add(Q16::from_ratio(2, 3))
    };
    let s = delta.div(max).unwrap_or(Q16::ZERO);
    [h.fract(), s, max]
}

/// Colour temperature in kelvin to linear RGB in 0..1.
///
/// A piecewise approximation of the Planckian locus, accurate enough for
/// lighting and cheap enough for the per-pixel path. Below 1000 K and above
/// 12000 K it clamps rather than extrapolating into negative channels.
pub fn kelvin_to_rgb(kelvin: Q16) -> [Q16; 3] {
    let k = kelvin.clamp(Q16::from_int(1000), Q16::from_int(12000));
    let hk = k.div(Q16::from_int(100)).unwrap_or(Q16::from_int(20));

    let red = if hk.0 <= Q16::from_int(66).0 {
        Q16::ONE
    } else {
        // 329.7 * (hk - 60)^-0.133, normalised to 0..1.
        let x = hk.sub(Q16::from_int(60));
        let f = x
            .pow(Q16::from_ratio(-1330, 10000))
            .unwrap_or(Q16::ONE)
            .mul(Q16::from_ratio(3297, 1000));
        f.div(Q16::from_int(255))
            .unwrap_or(Q16::ONE)
            .clamp(Q16::ZERO, Q16::ONE)
    };

    let green = if hk.0 <= Q16::from_int(66).0 {
        // 99.47 * ln(hk) - 161.12, normalised.
        let l = hk.ln().unwrap_or(Q16::ZERO).mul(Q16::from_ratio(9947, 100));
        l.sub(Q16::from_ratio(16112, 100))
            .div(Q16::from_int(255))
            .unwrap_or(Q16::ZERO)
            .clamp(Q16::ZERO, Q16::ONE)
    } else {
        let x = hk.sub(Q16::from_int(60));
        let f = x
            .pow(Q16::from_ratio(-755, 10000))
            .unwrap_or(Q16::ONE)
            .mul(Q16::from_ratio(2881, 10));
        f.div(Q16::from_int(255))
            .unwrap_or(Q16::ONE)
            .clamp(Q16::ZERO, Q16::ONE)
    };

    let blue = if hk.0 >= Q16::from_int(66).0 {
        Q16::ONE
    } else if hk.0 <= Q16::from_int(19).0 {
        Q16::ZERO
    } else {
        // 138.5 * ln(hk - 10) - 305.04, normalised.
        let l = hk
            .sub(Q16::from_int(10))
            .ln()
            .unwrap_or(Q16::ZERO)
            .mul(Q16::from_ratio(1385, 10));
        l.sub(Q16::from_ratio(30504, 100))
            .div(Q16::from_int(255))
            .unwrap_or(Q16::ZERO)
            .clamp(Q16::ZERO, Q16::ONE)
    };

    [red, green, blue]
}
