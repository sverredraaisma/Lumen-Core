//! Code generation.
//!
//! Walks a [`Resolved`] effect and emits bytecode for the three sections.
//!
//! # Where hoisting actually happens
//!
//! A `let` whose rate is [`Rate::Once`] or [`Rate::Frame`] is emitted into the
//! `frame` section and keeps its register for the whole frame. A `let` at
//! [`Rate::Pixel`] is emitted inline in `pixel`. Because the VM's register file
//! survives from `frame` into every pixel, a hoisted value is computed once and
//! read three hundred times — that is the payoff the whole design is arranged
//! around.
//!
//! # Determinism
//!
//! Everything here iterates in declaration order, never in the order of a hash
//! map. Identical source must give byte-identical bytecode, or reproducible
//! signed programs stop being reproducible.

use alloc::string::String;
use alloc::vec::Vec;

use lumen_vm::isa::{Instruction, OpCode, REG_COUNT};
use lumen_vm::program::builder::ProgramBuilder;
use lumen_vm::program::{Section, PALETTE_STOPS};
use lumen_vm::q16::Q16;
use lumen_vm::vm::{
    R_I, R_LX, R_LY, R_LZ, R_N, R_PREV, R_SCRATCH, R_T, R_U, R_UV_X, R_X, R_Y, R_Z,
};

use crate::ast::*;
use crate::diag::{Diagnostic, Diagnostics};
use crate::resolve::{builtin, core_fn, Rate, Resolved, SymbolKind};
use crate::BudgetReport;

/// A value in registers: a base register and how many it occupies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Val {
    base: u8,
    width: u8,
}

impl Val {
    fn scalar(base: u8) -> Val {
        Val { base, width: 1 }
    }

    /// Component `i`, saturating at the last one so a scalar broadcasts.
    fn at(self, i: u8) -> u8 {
        self.base + i.min(self.width - 1)
    }
}

/// The compiled artefact.
#[derive(Clone, PartialEq, Debug)]
pub struct Compiled {
    pub bytecode: Vec<u8>,
    pub report: BudgetReport,
}

/// Compile a resolved effect.
pub fn emit(resolved: &Resolved<'_>, diags: &mut Diagnostics) -> Option<Compiled> {
    let mut e = Emitter {
        r: resolved,
        diags,
        builder: ProgramBuilder::new(),
        frame: Vec::new(),
        pixel: Vec::new(),
        bound: Vec::new(),
        next_permanent: R_SCRATCH,
        temp_floor: R_SCRATCH,
        next_temp: R_SCRATCH,
        high_water: R_SCRATCH,
        inline_depth: 0,
        failed: false,
    };
    e.run();
    if e.failed || e.diags.has_errors() {
        return None;
    }

    let report = BudgetReport {
        instructions_per_pixel: e.pixel.iter().map(|i| i.op.cost()).sum(),
        instructions_per_frame: e.frame.iter().map(|i| i.op.cost()).sum(),
        instructions_once: 0,
        registers_used: e.high_water,
    };

    let mut builder = e.builder;
    for ins in &e.frame {
        builder.push(Section::Frame, *ins);
    }
    for ins in &e.pixel {
        builder.push(Section::Pixel, *ins);
    }
    builder.budget = report.instructions_per_pixel;
    builder.graph_hash = graph_hash(resolved);

    Some(Compiled {
        bytecode: builder.build(),
        report,
    })
}

/// A stable hash of the resolved effect, so an editor can recognise a program
/// already running on a device and skip the upload.
///
/// FNV-1a over the effect's name, version and stdlib. Deliberately not a hash of
/// the bytecode: the point is to identify the *source*, and two compilers at
/// different patch versions should still agree.
fn graph_hash(r: &Resolved<'_>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
    };
    feed(r.effect.name.as_bytes());
    feed(&r.effect.version.unwrap_or(0).to_le_bytes());
    feed(&r.stdlib.0.to_le_bytes());
    for l in &r.lets {
        feed(l.name.as_bytes());
    }
    for layer in &r.effect.layers {
        feed(layer.name.as_bytes());
        feed(&[layer.blend as u8]);
    }
    h
}

struct Emitter<'a, 'd> {
    r: &'a Resolved<'a>,
    diags: &'d mut Diagnostics,
    builder: ProgramBuilder,
    frame: Vec<Instruction>,
    pixel: Vec<Instruction>,
    /// Names bound to permanent registers, in declaration order.
    bound: Vec<(String, Val)>,
    next_permanent: u8,
    /// Where temporaries start. Everything below is permanent.
    temp_floor: u8,
    next_temp: u8,
    high_water: u8,
    inline_depth: u8,
    failed: bool,
}

/// How deep function inlining may nest.
///
/// Functions cannot recurse, so this only ever fires on a cycle or on genuinely
/// deep nesting; either way a diagnostic beats a stack overflow in the compiler.
const MAX_INLINE_DEPTH: u8 = 16;

impl Emitter<'_, '_> {
    fn run(&mut self) {
        self.emit_palettes();
        self.emit_frame_lets();
        // The accumulator has to be reserved BEFORE the temporary floor is set,
        // or temporaries alias it and the first blend stamps on the colour it is
        // supposed to be compositing onto.
        let Some(accum) = self.permanent(3) else {
            return;
        };
        // Effect-level `let`s that could not be hoisted still need registers
        // that survive the whole pixel, since layers read them.
        self.emit_pixel_lets();
        // Temporaries reuse everything above the permanents.
        self.temp_floor = self.next_permanent;
        self.next_temp = self.temp_floor;
        self.emit_pixel(accum);
    }

    /// Emit the effect-level `let`s that stayed at pixel rate.
    ///
    /// They live in permanent registers even though they are recomputed each
    /// pixel: several layers may read the same binding, and recomputing it per
    /// layer would silently multiply the cost of exactly the values the author
    /// pulled out into a `let` to avoid repeating.
    fn emit_pixel_lets(&mut self) {
        for l in &self.r.lets {
            if l.rate != Rate::Pixel {
                continue;
            }
            let width = l.ty.width() as u8;
            let Some(dst) = self.permanent(width) else {
                return;
            };
            let expr = &self.r.effect.lets[l.index].value;
            self.next_temp = self.next_permanent;
            let Some(v) = self.expr(Rate::Pixel, expr) else {
                return;
            };
            self.move_into(Rate::Pixel, dst, v);
            self.bound.push((l.name.clone(), dst));
        }
    }

    // ---- registers --------------------------------------------------------

    /// Reserve registers that survive from `frame` into every pixel.
    fn permanent(&mut self, width: u8) -> Option<Val> {
        if self.next_permanent as usize + width as usize > REG_COUNT {
            self.out_of_registers();
            return None;
        }
        let base = self.next_permanent;
        self.next_permanent += width;
        self.high_water = self.high_water.max(self.next_permanent);
        Some(Val { base, width })
    }

    /// Reserve scratch space for one expression.
    fn temp(&mut self, width: u8) -> Option<Val> {
        if self.next_temp as usize + width as usize > REG_COUNT {
            self.out_of_registers();
            return None;
        }
        let base = self.next_temp;
        self.next_temp += width;
        self.high_water = self.high_water.max(self.next_temp);
        Some(Val { base, width })
    }

    /// Give scratch back, without ever dropping below the floor.
    ///
    /// `high_water` is deliberately not lowered: the budget report should say
    /// how many registers the effect needed at its widest point, not how many
    /// happen to be live at the end.
    fn release_to(&mut self, mark: u8) {
        if mark >= self.temp_floor {
            self.next_temp = mark;
        }
    }

    fn out_of_registers(&mut self) {
        if !self.failed {
            self.failed = true;
            self.diags.push(Diagnostic::error(
                self.r.effect.span,
                "the effect needs more registers than the VM has",
                "split it into fewer simultaneous values, or move work into a `let` that can be hoisted out of the per-pixel section",
            ));
        }
    }

    fn push(&mut self, rate: Rate, ins: Instruction) {
        if rate == Rate::Pixel {
            self.pixel.push(ins);
        } else {
            self.frame.push(ins);
        }
    }

    fn constant(&mut self, v: f64) -> u16 {
        self.builder.constant(to_q16(v))
    }

    // ---- palettes ---------------------------------------------------------

    fn emit_palettes(&mut self) {
        for p in &self.r.palettes {
            let baked = bake_palette(p, self.diags);
            self.builder.palette(&baked);
        }
    }

    // ---- sections ---------------------------------------------------------

    fn emit_frame_lets(&mut self) {
        // Declaration order, so the bytecode is deterministic.
        for l in &self.r.lets {
            if l.rate == Rate::Pixel {
                continue;
            }
            let width = l.ty.width() as u8;
            let Some(dst) = self.permanent(width) else {
                return;
            };
            let expr = &self.r.effect.lets[l.index].value;
            self.next_temp = self.next_permanent;
            let Some(v) = self.expr(Rate::Frame, expr) else {
                return;
            };
            self.move_into(Rate::Frame, dst, v);
            self.bound.push((l.name.clone(), dst));
        }
    }

    fn emit_pixel(&mut self, accum: Val) {
        // Layers composite in declaration order: later layers sit on top.
        for (n, layer) in self.r.effect.layers.iter().enumerate() {
            self.emit_layer(layer, accum, n == 0);
        }
        self.pixel.push(Instruction::new(
            OpCode::EmitRgb,
            accum.at(0),
            accum.at(1),
            accum.at(2),
        ));
    }

    fn emit_layer(&mut self, layer: &Layer, accum: Val, first: bool) {
        // A mask gates the whole layer with a forward skip, so a masked-off
        // pixel costs a handful of instructions rather than the layer's whole
        // instruction count.
        let mask_at = layer.mask.as_ref().map(|name| {
            let start = self.pixel.len();
            let mval = self.mask_value(name);
            let reg = mval.map(|v| v.base).unwrap_or(0);
            self.pixel
                .push(Instruction::with_imm(OpCode::MaskTest, reg, 0));
            (start, self.pixel.len() - 1)
        });
        let body_start = self.pixel.len();

        // Layer-local `let`s are pixel-rate by construction: they are inside the
        // per-pixel section and cannot outlive it. Their registers are released
        // when the layer ends — without this the floor ratchets upward and a
        // handful of layers exhausts the register file.
        let saved_bound = self.bound.len();
        let saved_floor = self.temp_floor;
        for b in &layer.lets {
            self.next_temp = self.temp_floor;
            let Some(v) = self.expr(Rate::Pixel, &b.value) else {
                return;
            };
            // Park it above the temporaries so the next expression does not
            // stamp on it.
            let width = v.width;
            let Some(dst) = self.temp(width) else { return };
            self.move_into(Rate::Pixel, dst, v);
            self.temp_floor = self.next_temp;
            self.bound.push((b.name.clone(), dst));
        }

        let color_assign = layer.assigns.iter().find(|a| a.target == "color");
        let Some(assign) = color_assign else {
            return;
        };

        self.next_temp = self.temp_floor;
        let Some(color) = self.expr(Rate::Pixel, &assign.value) else {
            return;
        };

        // Opacity scales the layer's contribution before blending.
        let color = match &layer.opacity {
            Some(op) => {
                let Some(o) = self.expr(Rate::Pixel, op) else {
                    return;
                };
                let Some(scaled) = self.temp(3) else { return };
                for k in 0..3 {
                    self.pixel.push(Instruction::new(
                        OpCode::Mul,
                        scaled.base + k,
                        color.at(k),
                        o.base,
                    ));
                }
                scaled
            }
            None => color,
        };

        if first && layer.blend == Blend::Normal {
            self.move_into(Rate::Pixel, accum, color);
        } else {
            self.blend(layer.blend, accum, color);
        }

        // Patch the mask skip now that the body length is known.
        if let Some((_, idx)) = mask_at {
            let skip = (self.pixel.len() - body_start) as u16;
            self.pixel[idx] = Instruction::with_imm(OpCode::MaskTest, self.pixel[idx].a, skip);
        }
        self.bound.truncate(saved_bound);
        self.temp_floor = saved_floor;
        self.next_temp = saved_floor;
    }

    fn mask_value(&mut self, name: &str) -> Option<Val> {
        let idx = self.r.effect.masks.iter().position(|m| m.name == name)?;
        let expr = self.r.effect.masks[idx].value.clone();
        self.next_temp = self.temp_floor;
        let v = self.expr(Rate::Pixel, &expr)?;
        // No need to reserve this past the MASKTEST that reads it next: the
        // register is dead the moment the skip has been decided.
        Some(v)
    }

    /// Composite `src` onto `dst` in place.
    fn blend(&mut self, mode: Blend, dst: Val, src: Val) {
        for k in 0..3 {
            let d = dst.base + k;
            let s = src.at(k);
            match mode {
                Blend::Normal => {
                    self.pixel.push(Instruction::new(OpCode::Mov, d, s, 0));
                }
                Blend::Add => {
                    self.pixel.push(Instruction::new(OpCode::Add, d, d, s));
                }
                Blend::Multiply => {
                    self.pixel.push(Instruction::new(OpCode::Mul, d, d, s));
                }
                Blend::Max => {
                    self.pixel.push(Instruction::new(OpCode::Max, d, d, s));
                }
                Blend::Min => {
                    self.pixel.push(Instruction::new(OpCode::Min, d, d, s));
                }
                Blend::Difference => {
                    self.pixel.push(Instruction::new(OpCode::Sub, d, d, s));
                    self.pixel.push(Instruction::new(OpCode::Abs, d, d, 0));
                }
                Blend::Screen => {
                    // 1 - (1-a)(1-b), written with the two temporaries we have.
                    let Some(t) = self.temp(2) else { return };
                    let one = self.constant(1.0);
                    self.pixel
                        .push(Instruction::with_imm(OpCode::LoadK, t.base, one));
                    self.pixel
                        .push(Instruction::new(OpCode::Sub, t.base + 1, t.base, d));
                    let Some(t2) = self.temp(1) else { return };
                    self.pixel
                        .push(Instruction::new(OpCode::Sub, t2.base, t.base, s));
                    self.pixel.push(Instruction::new(
                        OpCode::Mul,
                        t.base + 1,
                        t.base + 1,
                        t2.base,
                    ));
                    self.pixel
                        .push(Instruction::new(OpCode::Sub, d, t.base, t.base + 1));
                }
                Blend::Overlay => {
                    // Approximated as 2ab, clamped. The exact form needs a
                    // branch per channel, which the pixel profile does not have.
                    let Some(t) = self.temp(2) else { return };
                    let two = self.constant(2.0);
                    let one = self.constant(1.0);
                    let zero = self.constant(0.0);
                    self.pixel
                        .push(Instruction::with_imm(OpCode::LoadK, t.base, two));
                    self.pixel.push(Instruction::new(OpCode::Mul, d, d, s));
                    self.pixel.push(Instruction::new(OpCode::Mul, d, d, t.base));
                    self.pixel
                        .push(Instruction::with_imm(OpCode::LoadK, t.base, zero));
                    self.pixel
                        .push(Instruction::with_imm(OpCode::LoadK, t.base + 1, one));
                    self.pixel
                        .push(Instruction::new(OpCode::Clamp, d, t.base, t.base + 1));
                }
            }
        }
    }

    fn move_into(&mut self, rate: Rate, dst: Val, src: Val) {
        for k in 0..dst.width {
            let s = src.at(k);
            if dst.base + k != s {
                self.push(rate, Instruction::new(OpCode::Mov, dst.base + k, s, 0));
            }
        }
    }

    // ---- expressions ------------------------------------------------------

    fn expr(&mut self, rate: Rate, e: &Expr) -> Option<Val> {
        match &e.kind {
            ExprKind::Number { value, .. } => {
                let k = self.constant(*value);
                let dst = self.temp(1)?;
                self.push(rate, Instruction::with_imm(OpCode::LoadK, dst.base, k));
                Some(dst)
            }
            ExprKind::Color(rgba) => {
                let dst = self.temp(3)?;
                for k in 0..3 {
                    let c = self.constant(rgba[k as usize]);
                    self.push(rate, Instruction::with_imm(OpCode::LoadK, dst.base + k, c));
                }
                Some(dst)
            }
            ExprKind::Str(_) => {
                self.diags.push(Diagnostic::error(
                    e.span,
                    "a string is not a value",
                    "strings are only used for names, authors and labels",
                ));
                None
            }
            ExprKind::Ident(name) => self.ident(rate, name, e),
            ExprKind::Field { base, field } => {
                let v = self.expr(rate, base)?;
                let k = match field.as_str() {
                    "x" | "u" | "r" => 0,
                    "y" | "v" | "g" => 1,
                    "z" | "b" => 2,
                    _ => 0,
                };
                Some(Val::scalar(v.at(k)))
            }
            ExprKind::Call { callee, args } => self.call(rate, callee, args, e),
            ExprKind::MethodCall { .. } => {
                self.diags.push(Diagnostic::error(
                    e.span,
                    "sim accessors are not implemented yet",
                    "remove the sim reference for now",
                ));
                None
            }
            ExprKind::Unary { op, operand } => {
                let v = self.expr(rate, operand)?;
                let dst = self.temp(v.width)?;
                for k in 0..v.width {
                    match op {
                        UnOp::Neg => self.push(
                            rate,
                            Instruction::new(OpCode::Neg, dst.base + k, v.at(k), 0),
                        ),
                        UnOp::Not => {
                            // `!x` is `x == 0`, which is branch-free.
                            let zero = self.constant(0.0);
                            let z = self.temp(1)?;
                            self.push(rate, Instruction::with_imm(OpCode::LoadK, z.base, zero));
                            self.push(
                                rate,
                                Instruction::new(OpCode::Eq, dst.base + k, v.at(k), z.base),
                            );
                        }
                    }
                }
                Some(dst)
            }
            ExprKind::Binary { op, lhs, rhs } => self.binary(rate, *op, lhs, rhs),
        }
    }

    fn ident(&mut self, rate: Rate, name: &str, e: &Expr) -> Option<Val> {
        // A `let` bound to a register.
        if let Some((_, v)) = self.bound.iter().rev().find(|(n, _)| n == name) {
            return Some(*v);
        }
        // A built-in reads straight out of its input register.
        if let Some(b) = builtin(name) {
            return Some(match b.name {
                "x" => Val::scalar(R_X),
                "y" => Val::scalar(R_Y),
                "z" => Val::scalar(R_Z),
                "lx" => Val::scalar(R_LX),
                "ly" => Val::scalar(R_LY),
                "lz" => Val::scalar(R_LZ),
                "i" => Val::scalar(R_I),
                "n" => Val::scalar(R_N),
                "u" => Val::scalar(R_U),
                "t" | "dt" => Val::scalar(R_T),
                "pos" => Val {
                    base: R_X,
                    width: 3,
                },
                "uv" => Val {
                    base: R_UV_X,
                    width: 2,
                },
                "prev" => Val {
                    base: R_PREV,
                    width: 1,
                },
                _ => {
                    // `mapq` has no register yet; zero is the honest answer for
                    // a device that has not been mapped.
                    let k = self.constant(0.0);
                    let dst = self.temp(1)?;
                    self.push(rate, Instruction::with_imm(OpCode::LoadK, dst.base, k));
                    dst
                }
            });
        }
        // A parameter is a compile-time constant: it changes between
        // activations, never within one.
        let sym = self.r.symbols.get(name)?;
        match sym.kind {
            SymbolKind::Param => {
                let p = &self.r.effect.params[sym.index];
                let width = p.ty.width() as u8;
                let dst = self.temp(width)?;
                let value = Self::const_expr_of(&p.default);
                for k in 0..width {
                    let c = self.constant(value[k as usize % value.len()]);
                    self.push(rate, Instruction::with_imm(OpCode::LoadK, dst.base + k, c));
                }
                Some(dst)
            }
            SymbolKind::Channel => {
                let slot = self.builder.channel(sym.index as u16);
                let dst = self.temp(1)?;
                self.push(rate, Instruction::new(OpCode::ChRead, dst.base, slot, 0));
                Some(dst)
            }
            SymbolKind::Palette => {
                // A bare palette name is only meaningful inside `palette(p, x)`,
                // which handles it directly.
                self.diags.push(Diagnostic::error(
                    e.span,
                    alloc::format!("`{name}` is a palette, not a value"),
                    "sample it with `palette(name, position)`",
                ));
                None
            }
            _ => {
                self.diags.push(Diagnostic::error(
                    e.span,
                    alloc::format!("`{name}` cannot be used here"),
                    "only parameters, channels, `let` bindings and built-ins are values",
                ));
                None
            }
        }
    }

    /// Evaluate a constant expression at compile time.
    ///
    /// Associated rather than a method: it needs nothing from the emitter, and
    /// taking `&self` would borrow the emitter across the recursion for no
    /// reason.
    fn const_expr_of(e: &Expr) -> Vec<f64> {
        match &e.kind {
            ExprKind::Number { value, .. } => alloc::vec![*value],
            ExprKind::Color(c) => alloc::vec![c[0], c[1], c[2]],
            ExprKind::Unary {
                op: UnOp::Neg,
                operand,
            } => Self::const_expr_of(operand).iter().map(|v| -v).collect(),
            ExprKind::Call { callee, args } if callee == "rgb" || callee == "vec3" => {
                args.iter().map(|a| Self::const_expr_of(a)[0]).collect()
            }
            _ => alloc::vec![0.0],
        }
    }

    /// Inline a user function.
    ///
    /// Functions are the text form of an encapsulated node group, and they are
    /// **always inlined**: no recursion, no function pointers, no dynamic
    /// dispatch. Inlining is what keeps the instruction count static, which is
    /// what keeps the budget check exact - a call would make cost depend on a
    /// path taken at runtime, and the whole publish-time budget answer rests on
    /// that not being true.
    fn inline_fn(&mut self, rate: Rate, index: usize, args: &[Expr], e: &Expr) -> Option<Val> {
        // A function that calls itself, directly or through another, would
        // inline forever. The depth cap turns that into a diagnostic instead of
        // a stack overflow in the compiler.
        if self.inline_depth >= MAX_INLINE_DEPTH {
            self.diags.push(Diagnostic::error(
                e.span,
                "functions nest too deeply, or call each other in a cycle",
                "functions are always inlined, so they cannot recurse - rewrite the expression without the cycle",
            ));
            self.failed = true;
            return None;
        }

        let decl = &self.r.effect.fns[index];
        if decl.params.len() != args.len() {
            self.diags.push(Diagnostic::error(
                e.span,
                alloc::format!(
                    "`{}` takes {} arguments, but {} were given",
                    decl.name,
                    decl.params.len(),
                    args.len()
                ),
                "check the argument list against the function declaration",
            ));
            return None;
        }

        // Evaluate the arguments in the caller's scope, then bind them to the
        // parameter names for the body. Evaluating first is what makes an
        // argument expression cost once however many times the parameter is
        // mentioned in the body.
        let mut bindings = Vec::new();
        for (arg, (name, _)) in args.iter().zip(&decl.params) {
            let v = self.expr(rate, arg)?;
            bindings.push((name.clone(), v));
        }

        let saved = self.bound.len();
        for b in bindings {
            self.bound.push(b);
        }
        self.inline_depth += 1;

        // The function's own `let`s, then its return expression.
        let decl = &self.r.effect.fns[index];
        let lets = decl.lets.clone();
        let body = decl.body.clone();
        let mut ok = true;
        for l in &lets {
            match self.expr(rate, &l.value) {
                Some(v) => self.bound.push((l.name.clone(), v)),
                None => {
                    ok = false;
                    break;
                }
            }
        }
        let out = if ok { self.expr(rate, &body) } else { None };

        self.inline_depth -= 1;
        self.bound.truncate(saved);
        out
    }

    fn call(&mut self, rate: Rate, callee: &str, args: &[Expr], e: &Expr) -> Option<Val> {
        // `palette` takes a palette name, not a value, so it cannot go through
        // the ordinary argument path.
        if callee == "palette" {
            let ExprKind::Ident(pname) = &args[0].kind else {
                self.diags.push(Diagnostic::error(
                    args[0].span,
                    "the first argument to `palette` must be a palette name",
                    "palettes are referenced by identifier, never by string",
                ));
                return None;
            };
            let idx = self.r.palettes.iter().position(|p| &p.name == pname)?;
            let pos = self.expr(rate, &args[1])?;
            let dst = self.temp(3)?;
            self.push(
                rate,
                Instruction::new(OpCode::Palette, dst.base, pos.base, idx as u8),
            );
            return Some(dst);
        }

        // A user function is inlined with its arguments bound by name, so it
        // cannot go through the ordinary "evaluate args into registers" path.
        if core_fn(callee).is_none() {
            let index = self.r.effect.fns.iter().position(|f| f.name == callee)?;
            return self.inline_fn(rate, index, args, e);
        }

        let mark = self.next_temp;
        let mut vals = Vec::new();
        for a in args {
            vals.push(self.expr(rate, a)?);
        }

        // Constructors are register moves.
        match callee {
            "vec2" | "vec3" | "rgb" => {
                let width = vals.len() as u8;
                let dst = self.temp(width)?;
                for (k, v) in vals.iter().enumerate() {
                    self.push(
                        rate,
                        Instruction::new(OpCode::Mov, dst.base + k as u8, v.base, 0),
                    );
                }
                return Some(dst);
            }
            "hsv" => {
                let packed = self.temp(3)?;
                for (k, v) in vals.iter().enumerate() {
                    self.push(
                        rate,
                        Instruction::new(OpCode::Mov, packed.base + k as u8, v.base, 0),
                    );
                }
                let dst = self.temp(3)?;
                self.push(
                    rate,
                    Instruction::new(OpCode::Hsv2Rgb, dst.base, packed.base, 0),
                );
                return Some(dst);
            }
            "temp" => {
                let dst = self.temp(3)?;
                self.push(
                    rate,
                    Instruction::new(OpCode::Temp2Rgb, dst.base, vals[0].base, 0),
                );
                // Intensity scales the result.
                for k in 0..3 {
                    self.push(
                        rate,
                        Instruction::new(OpCode::Mul, dst.base + k, dst.base + k, vals[1].base),
                    );
                }
                return Some(dst);
            }
            "noise3" => {
                // NOISE3 reads three consecutive registers, so pack first.
                let packed = self.temp(3)?;
                for (k, v) in vals.iter().enumerate() {
                    self.push(
                        rate,
                        Instruction::new(OpCode::Mov, packed.base + k as u8, v.base, 0),
                    );
                }
                let dst = self.temp(1)?;
                self.push(
                    rate,
                    Instruction::new(OpCode::Noise3, dst.base, packed.base, 0),
                );
                return Some(dst);
            }
            "length" if vals.len() == 3 => {
                let packed = self.temp(3)?;
                for (k, v) in vals.iter().enumerate() {
                    self.push(
                        rate,
                        Instruction::new(OpCode::Mov, packed.base + k as u8, v.base, 0),
                    );
                }
                let dst = self.temp(1)?;
                self.push(
                    rate,
                    Instruction::new(OpCode::Len3, dst.base, packed.base, 0),
                );
                return Some(dst);
            }
            _ => {}
        }

        // The straightforward one-instruction cases. Each reads its sources and
        // writes its destination, so reusing the argument scratch is safe as
        // long as every argument is a scalar.
        if vals.iter().all(|v| v.width == 1) {
            self.release_to(mark);
        }
        let dst = self.temp(1)?;
        let ins = match callee {
            "abs" => Instruction::new(OpCode::Abs, dst.base, vals[0].base, 0),
            "floor" => Instruction::new(OpCode::Floor, dst.base, vals[0].base, 0),
            "fract" => Instruction::new(OpCode::Fract, dst.base, vals[0].base, 0),
            "sqrt" => Instruction::new(OpCode::Sqrt, dst.base, vals[0].base, 0),
            "sin" => Instruction::new(OpCode::Sin, dst.base, vals[0].base, 0),
            "cos" => Instruction::new(OpCode::Cos, dst.base, vals[0].base, 0),
            "sin01" => Instruction::new(OpCode::SinTurns, dst.base, vals[0].base, 0),
            "cos01" => Instruction::new(OpCode::CosTurns, dst.base, vals[0].base, 0),
            "exp" => Instruction::new(OpCode::Exp, dst.base, vals[0].base, 0),
            "log" => Instruction::new(OpCode::Log, dst.base, vals[0].base, 0),
            "pow" => Instruction::new(OpCode::Pow, dst.base, vals[0].base, vals[1].base),
            "atan2" => Instruction::new(OpCode::Atan2, dst.base, vals[0].base, vals[1].base),
            "min" => Instruction::new(OpCode::Min, dst.base, vals[0].base, vals[1].base),
            "max" => Instruction::new(OpCode::Max, dst.base, vals[0].base, vals[1].base),
            "step" => Instruction::new(OpCode::Step, dst.base, vals[0].base, vals[1].base),
            "noise1" => Instruction::new(OpCode::Noise1, dst.base, vals[0].base, 0),
            "noise2" => Instruction::new(OpCode::Noise2, dst.base, vals[0].base, vals[1].base),
            "length" => Instruction::new(OpCode::Len2, dst.base, vals[0].base, vals[1].base),
            "dot" => Instruction::new(OpCode::Mul, dst.base, vals[0].base, vals[1].base),
            "clamp" | "smoothstep" | "mix" | "select" => {
                // Three-operand forms where the destination is also the first
                // source, so the first argument moves in before the operation.
                self.push(
                    rate,
                    Instruction::new(OpCode::Mov, dst.base, vals[0].base, 0),
                );
                let op = match callee {
                    "clamp" => OpCode::Clamp,
                    "smoothstep" => OpCode::SmoothStep,
                    "mix" => OpCode::Lerp,
                    _ => OpCode::Select,
                };
                Instruction::new(op, dst.base, vals[1].base, vals[2].base)
            }
            other => {
                self.diags.push(Diagnostic::error(
                    e.span,
                    alloc::format!("`{other}` is not implemented yet"),
                    "it is in the language but not yet in the emitter",
                ));
                return None;
            }
        };
        self.push(rate, ins);
        Some(dst)
    }

    fn binary(&mut self, rate: Rate, op: BinOp, lhs: &Expr, rhs: &Expr) -> Option<Val> {
        let mark = self.next_temp;
        let a = self.expr(rate, lhs)?;
        let b = self.expr(rate, rhs)?;
        let width = a.width.max(b.width);
        // Once both operands are in registers, their scratch is dead and the
        // result can reuse it. Only safe for the all-scalar case: with a
        // broadcasting operand the destination would overwrite a value later
        // components still need to read. Scalars are the overwhelming majority
        // of an effect, and without this a handful of layers exhausts the file.
        let dst = if width == 1 && a.width == 1 && b.width == 1 {
            self.release_to(mark);
            self.temp(1)?
        } else {
            self.temp(width)?
        };
        for k in 0..width {
            let (x, y) = (a.at(k), b.at(k));
            let d = dst.base + k;
            let opcode = match op {
                BinOp::Add => OpCode::Add,
                BinOp::Sub => OpCode::Sub,
                BinOp::Mul => OpCode::Mul,
                BinOp::Div => OpCode::Div,
                BinOp::Lt => OpCode::Lt,
                BinOp::Le => OpCode::Le,
                BinOp::Gt => OpCode::Gt,
                BinOp::Ge => OpCode::Ge,
                BinOp::Eq => OpCode::Eq,
                BinOp::Ne => {
                    // `a != b` is `!(a == b)`, and `!` on a 0/1 value is
                    // `1 - it`, which stays branch-free.
                    self.push(rate, Instruction::new(OpCode::Eq, d, x, y));
                    let one = self.constant(1.0);
                    let t = self.temp(1)?;
                    self.push(rate, Instruction::with_imm(OpCode::LoadK, t.base, one));
                    self.push(rate, Instruction::new(OpCode::Sub, d, t.base, d));
                    continue;
                }
                // Both operands are 0 or 1, so `min` is `and` and `max` is `or`.
                BinOp::And => OpCode::Min,
                BinOp::Or => OpCode::Max,
                BinOp::Rem => {
                    // `a % b` as `a - floor(a/b)*b`.
                    let t = self.temp(2)?;
                    self.push(rate, Instruction::new(OpCode::Div, t.base, x, y));
                    self.push(rate, Instruction::new(OpCode::Floor, t.base, t.base, 0));
                    self.push(rate, Instruction::new(OpCode::Mul, t.base + 1, t.base, y));
                    self.push(rate, Instruction::new(OpCode::Sub, d, x, t.base + 1));
                    continue;
                }
            };
            self.push(rate, Instruction::new(opcode, d, x, y));
        }
        Some(dst)
    }
}

fn to_q16(v: f64) -> Q16 {
    let scaled = v * 65536.0;
    let clamped = if scaled > i32::MAX as f64 {
        i32::MAX as f64
    } else if scaled < i32::MIN as f64 {
        i32::MIN as f64
    } else {
        scaled
    };
    // Round half away from zero, without `f64::round`, which is std-only.
    let r = if clamped >= 0.0 {
        (clamped + 0.5) as i32
    } else {
        (clamped - 0.5) as i32
    };
    Q16(r)
}

/// Resolve a palette's stops into the fixed-size lookup table the VM samples.
///
/// Interpolation happens **in the declared space** and the result is baked to
/// linear RGB, so the choice of space costs nothing at runtime — which is what
/// lets `oklab` be the default without anyone paying for it per pixel.
fn bake_palette(p: &Palette, diags: &mut Diagnostics) -> [(Q16, Q16, Q16); PALETTE_STOPS] {
    let mut stops: Vec<(f64, [f64; 3])> = Vec::new();
    for s in &p.stops {
        let rgb = match &s.color.kind {
            ExprKind::Color(c) => [c[0], c[1], c[2]],
            _ => {
                diags.push(Diagnostic::error(
                    s.color.span,
                    "a palette stop must be a colour literal",
                    "write a hex colour like `#ff8000`",
                ));
                [0.0, 0.0, 0.0]
            }
        };
        stops.push((s.position, rgb));
    }
    if stops.is_empty() {
        diags.push(Diagnostic::error(
            p.span,
            alloc::format!("palette `{}` has no stops", p.name),
            "add stops like `0 #000000` and `1 #ffffff`",
        ));
        stops.push((0.0, [0.0, 0.0, 0.0]));
    }
    // Sorting by position makes the result independent of declaration order,
    // which keeps compilation deterministic for a palette written out of order.
    stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));

    let mut out = [(Q16::ZERO, Q16::ZERO, Q16::ZERO); PALETTE_STOPS];
    for (i, slot) in out.iter_mut().enumerate() {
        let pos = i as f64 / PALETTE_STOPS as f64;
        let rgb = sample_stops(&stops, pos, p.space);
        *slot = (to_q16(rgb[0]), to_q16(rgb[1]), to_q16(rgb[2]));
    }
    out
}

fn sample_stops(stops: &[(f64, [f64; 3])], pos: f64, space: ColorSpace) -> [f64; 3] {
    if pos <= stops[0].0 {
        return stops[0].1;
    }
    if pos >= stops[stops.len() - 1].0 {
        return stops[stops.len() - 1].1;
    }
    for w in stops.windows(2) {
        let (p0, c0) = w[0];
        let (p1, c1) = w[1];
        if pos >= p0 && pos <= p1 {
            let span = p1 - p0;
            let t = if span <= 0.0 { 0.0 } else { (pos - p0) / span };
            return mix_in_space(c0, c1, t, space);
        }
    }
    stops[stops.len() - 1].1
}

fn mix_in_space(a: [f64; 3], b: [f64; 3], t: f64, space: ColorSpace) -> [f64; 3] {
    match space {
        ColorSpace::LinearRgb => [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
        ],
        // Oklab is the default because interpolating there is what makes a
        // gradient look even rather than passing through a muddy middle.
        _ => {
            let la = linear_to_oklab(a);
            let lb = linear_to_oklab(b);
            let m = [
                la[0] + (lb[0] - la[0]) * t,
                la[1] + (lb[1] - la[1]) * t,
                la[2] + (lb[2] - la[2]) * t,
            ];
            oklab_to_linear(m)
        }
    }
}

fn cbrt(x: f64) -> f64 {
    if x == 0.0 {
        return 0.0;
    }
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let a = x * sign;
    // Newton from a rough seed; converges in a handful of steps on this domain.
    let mut y = a;
    for _ in 0..24 {
        y = (2.0 * y + a / (y * y)) / 3.0;
    }
    sign * y
}

fn linear_to_oklab(c: [f64; 3]) -> [f64; 3] {
    let l = 0.4122214708 * c[0] + 0.5363325363 * c[1] + 0.0514459929 * c[2];
    let m = 0.2119034982 * c[0] + 0.6806995451 * c[1] + 0.1073969566 * c[2];
    let s = 0.0883024619 * c[0] + 0.2817188376 * c[1] + 0.6299787005 * c[2];
    let l_ = cbrt(l);
    let m_ = cbrt(m);
    let s_ = cbrt(s);
    [
        0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_,
        1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_,
        0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757660 * s_,
    ]
}

fn oklab_to_linear(c: [f64; 3]) -> [f64; 3] {
    let l_ = c[0] + 0.3963377774 * c[1] + 0.2158037573 * c[2];
    let m_ = c[0] - 0.1055613458 * c[1] - 0.0638541728 * c[2];
    let s_ = c[0] - 0.0894841775 * c[1] - 1.2914855480 * c[2];
    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;
    [
        (4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s).clamp(0.0, 1.0),
        (-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s).clamp(0.0, 1.0),
        (-0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s).clamp(0.0, 1.0),
    ]
}
