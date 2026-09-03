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
    /// The simulation's own program, when the effect declares one with a body.
    ///
    /// A second artefact rather than a section of the first, because only the
    /// **sim master** ever runs it while every device runs the pixel program.
    /// Shipping one program would mean every device carrying code it must never
    /// execute, and the profile check that keeps `ASTORE` out of a pixel kernel
    /// is a property of a whole program rather than of a section.
    pub sim: Option<Vec<u8>>,
}

/// Compile a resolved effect.
/// The array a sim's element positions occupy.
///
/// Flat `q16` addressed by `(array, index)`, so element *k* has its position at
/// `3k`, `3k+1`, `3k+2`. One array per field keeps the addressing a multiply and
/// an add, and lets a simulation broadcast only the fields its accessors read.
const SIM_POS_ARRAY: u8 = 0;

pub fn emit(resolved: &Resolved<'_>, diags: &mut Diagnostics) -> Option<Compiled> {
    let mut e = Emitter {
        r: resolved,
        diags,
        builder: ProgramBuilder::new(),
        once: Vec::new(),
        frame: Vec::new(),
        pixel: Vec::new(),
        bound: Vec::new(),
        next_permanent: R_SCRATCH,
        temp_floor: R_SCRATCH,
        next_temp: R_SCRATCH,
        high_water: R_SCRATCH,
        inline_depth: 0,
        failed: false,
        sim_body: Vec::new(),
        in_sim: false,
        sim_builder: ProgramBuilder::new(),
    };

    // The body of a `sim` is understood by `resolve` and cannot be lowered yet.
    // Refused here rather than there, so an author still gets every real
    // complaint about what they wrote - an unknown name, a missing `count`, a
    // field assigned outside a `foreach` - instead of one blanket refusal that
    // hides all of them. What is missing is code generation, and this is where
    // code generation says so.
    for sim in &resolved.effect.sims {
        // An **empty** body is a declaration of shape rather than a simulation:
        // "a simulation of this many elements arrives here". Nothing to lower,
        // and the accessors only need the count. That is how a device that
        // *receives* a simulation it does not run declares what it is
        // receiving - the case a `sim<..>` channel cannot serve, since it names
        // a record type and carries no count.
        if sim.body.is_empty() {
            continue;
        }
        if resolved.effect.sims.len() > 1 {
            e.diags.push(Diagnostic::error(
                sim.span,
                "an effect may run only one `sim`",
                "the device runs one simulation program; combine them, or leave all but one body empty to declare simulations this device only reads",
            ));
            e.failed = true;
            break;
        }
    }

    e.run();
    if e.failed || e.diags.has_errors() {
        return None;
    }

    // After the pixel program, so a failure in the sim body does not lose the
    // diagnostics the layers produced.
    let sim = emit_sim_program(&mut e, resolved);
    if e.failed || e.diags.has_errors() {
        return None;
    }

    let report = BudgetReport {
        instructions_per_pixel: e.pixel.iter().map(|i| i.op.cost()).sum(),
        instructions_per_frame: e.frame.iter().map(|i| i.op.cost()).sum(),
        instructions_once: e.once.iter().map(|i| i.op.cost()).sum(),
        registers_used: e.high_water,
        fps: e.r.effect.fps,
    };

    let mut builder = e.builder;
    for ins in &e.once {
        builder.push(Section::Once, *ins);
    }
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
        sim,
    })
}

/// Build the simulation's own program, if the effect declares one with a body.
///
/// A separate artefact because only the sim master runs it. Its instructions go
/// in the `frame` section: a sim body runs once per frame on one device, which
/// is what that section is.
fn emit_sim_program(e: &mut Emitter<'_, '_>, resolved: &Resolved<'_>) -> Option<Vec<u8>> {
    let sim = resolved.effect.sims.iter().find(|s| !s.body.is_empty())?;
    let declared = resolved.sims.iter().find(|s| s.name == sim.name)?;

    // The field list comes from `resolve`, which is the only place it is
    // worked out. Recomputing it here is how the two came to disagree: the
    // emitter counted only assigned fields, so a body reading `p.vel` without
    // writing it resolved and then silently failed to emit.
    if !e.emit_sim(sim, declared.count, &declared.fields) {
        return None;
    }

    let mut builder = core::mem::replace(&mut e.sim_builder, ProgramBuilder::new());
    builder.profile_sim = true;
    for ins in &e.sim_body {
        builder.push(Section::Frame, *ins);
    }
    builder.budget = e.sim_body.iter().map(|i| i.op.cost()).sum();
    Some(builder.build())
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
    /// The `once` section. Empty today - nothing the language expresses needs
    /// activation-time work yet - but counted rather than assumed zero, so the
    /// report stays honest the moment something does.
    once: Vec<Instruction>,
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
    /// Instructions for the simulation's own program, while one is being built.
    ///
    /// A third target for [`Emitter::push`], selected by `in_sim` rather than by
    /// rate: everything in a sim body runs at the same time, once per frame on
    /// one device, so rate says nothing useful about where it goes.
    sim_body: Vec<Instruction>,
    in_sim: bool,
    /// The simulation program's own constant pool.
    ///
    /// Separate because the two are separate programs: a `LOAD_K` immediate is
    /// an index into *its own* program's pool, so routing sim constants into
    /// the pixel program's builder produced a sim program whose immediates
    /// pointed into a pool it did not have — which the VM rejected, correctly,
    /// as a bad program.
    sim_builder: ProgramBuilder,
}

/// How deep function inlining may nest.
///
/// Functions cannot recurse, so this only ever fires on a cycle or on genuinely
/// deep nesting; either way a diagnostic beats a stack overflow in the compiler.
const MAX_INLINE_DEPTH: u8 = 16;

/// For each layer-local `let`, the index of the last statement that reads it.
///
/// `usize::MAX` means "to the end of the layer": read by the colour assign, a
/// `state` write, the opacity or the mask, all of which come after every `let`.
/// Anything else is the index of the latest later `let` that mentions it.
///
/// Conservative by construction — a value that is actually dead may be reported
/// live, which costs a register — because the other direction emits a read of a
/// register something else has overwritten, and that renders as a colour nobody
/// chose.
fn layer_let_last_use(layer: &Layer) -> Vec<usize> {
    let mut out = alloc::vec![usize::MAX; layer.lets.len()];
    for (i, b) in layer.lets.iter().enumerate() {
        // Anything after the `let` block keeps it alive to the end.
        let after_the_lets = layer
            .assigns
            .iter()
            .any(|a| crate::ast::mentions(&a.value, &b.name))
            || layer
                .opacity
                .as_ref()
                .is_some_and(|o| crate::ast::mentions(o, &b.name));
        if after_the_lets {
            continue;
        }
        // Otherwise the last later `let` that mentions it, or itself if none
        // does — an unread `let` is dead the moment it is written, and the
        // resolver has already warned about it.
        let mut last = i;
        for (j, other) in layer.lets.iter().enumerate().skip(i + 1) {
            if crate::ast::mentions(&other.value, &b.name) {
                last = j;
            }
        }
        out[i] = last;
    }
    out
}

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
            if l.rate != Rate::Pixel || !l.used {
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

    // ---- the simulation's own program -------------------------------------

    /// Lower a `sim` body into the instructions of its own program.
    ///
    /// Returns `false` if anything in the body has no lowering, having already
    /// said which.
    fn emit_sim(&mut self, sim: &crate::ast::Sim, count: u32, fields: &[String]) -> bool {
        // A fresh register file: the sim program is a separate artefact and
        // shares nothing with the pixel program's allocation.
        let saved = (self.next_permanent, self.temp_floor, self.next_temp);
        self.next_permanent = R_SCRATCH;
        self.temp_floor = R_SCRATCH;
        self.next_temp = R_SCRATCH;
        self.in_sim = true;

        let ok = self.sim_stmts(&sim.body, sim, count, fields);

        self.in_sim = false;
        let (p, f, t) = saved;
        self.next_permanent = p;
        self.temp_floor = f;
        self.next_temp = t;
        ok
    }

    fn sim_stmts(
        &mut self,
        body: &[SimStmt],
        sim: &crate::ast::Sim,
        count: u32,
        fields: &[String],
    ) -> bool {
        for stmt in body {
            match stmt {
                SimStmt::Let(b) => {
                    let Some(v) = self.expr(Rate::Frame, &b.value) else {
                        return false;
                    };
                    let Some(dst) = self.permanent(v.width) else {
                        return false;
                    };
                    for k in 0..v.width {
                        self.push(
                            Rate::Frame,
                            Instruction::new(OpCode::Mov, dst.base + k, v.base + k, 0),
                        );
                    }
                    self.bound.push((b.name.clone(), dst));
                }
                SimStmt::Assign(a) if a.field.is_none() => {
                    let Some(v) = self.expr(Rate::Frame, &a.value) else {
                        return false;
                    };
                    let Some(dst) = self
                        .bound
                        .iter()
                        .find(|(n, _)| n == &a.target)
                        .map(|(_, v)| *v)
                    else {
                        return false;
                    };
                    for k in 0..dst.width.min(v.width) {
                        self.push(
                            Rate::Frame,
                            Instruction::new(OpCode::Mov, dst.base + k, v.base + k, 0),
                        );
                    }
                }
                // A field assignment outside a `foreach` has no element to write
                // to; `resolve` has already said so.
                SimStmt::Assign(_) => return false,
                SimStmt::If { span, .. } => {
                    // `MASK_TEST` can express this - it skips forward when a
                    // register is zero - but a forward skip needs its distance
                    // patched once the branch is emitted, and doing that
                    // carelessly is how a compiler starts producing plausible
                    // wrong code. A separate change, with its own tests.
                    self.diags.push(Diagnostic::error(
                        *span,
                        "`if` inside a `sim` cannot be compiled yet",
                        "write the branch arithmetically with `select` or `step`, which is what the pixel profile does",
                    ));
                    return false;
                }
                SimStmt::ForEach { binding, body, .. } => {
                    // Unrolled over the count, for the same reason the accessors
                    // are: the trip count is a compile-time constant, so the
                    // program stays costable and needs no loop machinery.
                    for k in 0..count {
                        if !self.sim_element(binding, body, k, sim, count, fields) {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }

    /// One unrolled iteration: bind the element's fields, run the body, write
    /// back whatever it assigned.
    fn sim_element(
        &mut self,
        binding: &str,
        body: &[SimStmt],
        k: u32,
        sim: &crate::ast::Sim,
        count: u32,
        fields: &[String],
    ) -> bool {
        let mark = self.next_temp;
        let bound_before = self.bound.len();

        // Load every field of this element into registers. Loading all of them
        // rather than only those read is what keeps the write-back below
        // simple, and a field nobody touches costs three loads once per element
        // rather than anything per pixel.
        let mut slots = Vec::new();
        for (array, field) in fields.iter().enumerate() {
            let Some(v) = self.temp(3) else {
                return false;
            };
            let idx = match self.temp(1) {
                Some(i) => i,
                None => return false,
            };
            for lane in 0..3u8 {
                let c = self.constant((k * 3 + lane as u32) as f64);
                self.push(
                    Rate::Frame,
                    Instruction::with_imm(OpCode::LoadK, idx.base, c),
                );
                self.push(
                    Rate::Frame,
                    Instruction::new(OpCode::ALoad, v.base + lane, array as u8, idx.base),
                );
            }
            self.bound.push((alloc::format!("{binding}.{field}"), v));
            slots.push((array as u8, v));
        }

        let ok = self.sim_stmts_in_element(body, sim, count, fields, binding);

        if ok {
            // Write back, in the same order, so a field read by a later element
            // sees this one's update - which is what makes the unrolled loop
            // mean the same thing as a real one.
            for (array, v) in &slots {
                let idx = match self.temp(1) {
                    Some(i) => i,
                    None => return false,
                };
                for lane in 0..3u8 {
                    let c = self.constant((k * 3 + lane as u32) as f64);
                    self.push(
                        Rate::Frame,
                        Instruction::with_imm(OpCode::LoadK, idx.base, c),
                    );
                    // `ASTORE` is not `ALOAD` with the operands in the same
                    // places: it takes the array in `a`, the index register in
                    // `b` and the value in `c`, where `ALOAD` puts the
                    // destination in `a`. Getting it the other way round stored
                    // into "array 15" - the value register, read as an array id.
                    self.push(
                        Rate::Frame,
                        Instruction::new(OpCode::AStore, *array, idx.base, v.base + lane),
                    );
                }
            }
        }

        self.bound.truncate(bound_before);
        self.release_to(mark);
        ok
    }

    /// The body of a `foreach`, where `p.field` names a bound register.
    fn sim_stmts_in_element(
        &mut self,
        body: &[SimStmt],
        sim: &crate::ast::Sim,
        count: u32,
        fields: &[String],
        binding: &str,
    ) -> bool {
        for stmt in body {
            if let SimStmt::Assign(a) = stmt {
                if let Some(field) = &a.field {
                    if a.target != binding {
                        return false;
                    }
                    let Some(v) = self.expr(Rate::Frame, &a.value) else {
                        return false;
                    };
                    let name = alloc::format!("{binding}.{field}");
                    let Some(dst) = self.bound.iter().find(|(n, _)| n == &name).map(|(_, v)| *v)
                    else {
                        return false;
                    };
                    // A scalar assigned to a three-lane field fills all three,
                    // which is what `p.vel = 0` has to mean.
                    for lane in 0..3u8 {
                        let src = if v.width == 1 { v.base } else { v.base + lane };
                        self.push(
                            Rate::Frame,
                            Instruction::new(OpCode::Mov, dst.base + lane, src, 0),
                        );
                    }
                    continue;
                }
            }
            if !self.sim_stmts(core::slice::from_ref(stmt), sim, count, fields) {
                return false;
            }
        }
        true
    }

    // ---- sim accessors ----------------------------------------------------

    /// Lower `<sim>.influence(p, r)`, `<sim>.nearest(p)` or `<sim>.field(p)`.
    ///
    /// Unrolled over the element count, which is a compile-time constant — the
    /// reason `ALEN` stayed sim-only is that a trip count read from the array
    /// would not be costable, and the budget check would stop being exact.
    ///
    /// Unrolling rather than looping is deliberate. A `REPEAT` loop keeps the
    /// program small and needs control flow this emitter has never had; an
    /// unrolled accumulation is straight-line code it can already produce, and
    /// its cost lands in `lumen budget` where an author will see it. A per-pixel
    /// accessor over N elements costs about N times its body, so a handful of
    /// elements is affordable and sixty-four is not — and the budget check
    /// refuses the second at compile time rather than letting it stutter.
    fn sim_accessor(
        &mut self,
        rate: Rate,
        e: &Expr,
        base: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Option<Val> {
        let ExprKind::Ident(name) = &base.kind else {
            self.diags.push(Diagnostic::error(
                base.span,
                "an accessor needs a sim to read",
                "call it on the name of a `sim` block, as in `swarm.nearest(p)`",
            ));
            return None;
        };

        let count = self.sim_count(name, e)?;
        if !self.r.sim_has_pos(name) {
            self.diags.push(Diagnostic::error(
                e.span,
                alloc::format!("`{name}` has no `pos` field to measure against"),
                "an accessor measures distance to each element's `pos`; assign one in the sim, as in `p.pos = ...`",
            ));
            return None;
        }

        // The point being asked about, evaluated once however many elements
        // there are.
        let point = self.expr(rate, args.first()?)?;
        if point.width != 3 {
            return None;
        }

        match method {
            "nearest" => self.emit_nearest(rate, point, count),
            "influence" => {
                let radius = self.expr(rate, args.get(1)?)?;
                self.emit_influence(rate, point, radius, count)
            }
            "field" => self.emit_field(rate, point, count),
            _ => None,
        }
    }

    /// The element count of `name`, or `None` with a diagnostic already pushed.
    fn sim_count(&mut self, name: &str, e: &Expr) -> Option<u32> {
        match self.r.sim_element_count(name) {
            Some(count) => Some(count),
            None => {
                self.diags.push(Diagnostic::error(
                    e.span,
                    alloc::format!("`{name}` does not declare how many elements it has"),
                    "accessors need a bound they can be costed against; declare the simulation with `sim name(count = ..)` in this effect",
                ));
                None
            }
        }
    }

    /// `dst = p - pos[k]`, the vector from element `k` to the point.
    fn emit_offset(&mut self, rate: Rate, point: Val, k: u32) -> Option<Val> {
        let off = self.temp(3)?;
        // One index register for all three lanes. Allocating it inside the loop
        // took three, which with the offset and an accumulator was enough to run
        // a two-element `influence` out of registers - and registers, not
        // instructions, are the binding constraint on this VM.
        let idx = self.temp(1)?;
        for lane in 0..3u8 {
            let index = self.constant((k * 3 + lane as u32) as f64);
            // The index is a value at run time even though it is constant here,
            // because `ALOAD` takes it in a register.
            self.push(rate, Instruction::with_imm(OpCode::LoadK, idx.base, index));
            self.push(
                rate,
                Instruction::new(OpCode::ALoad, off.base + lane, SIM_POS_ARRAY, idx.base),
            );
            self.push(
                rate,
                Instruction::new(
                    OpCode::Sub,
                    off.base + lane,
                    point.base + lane,
                    off.base + lane,
                ),
            );
        }
        Some(off)
    }

    fn emit_nearest(&mut self, rate: Rate, point: Val, count: u32) -> Option<Val> {
        let best = self.temp(1)?;
        for k in 0..count {
            let mark = self.next_temp;
            let off = self.emit_offset(rate, point, k)?;
            let d = self.temp(1)?;
            self.push(rate, Instruction::new(OpCode::Len3, d.base, off.base, 0));
            if k == 0 {
                self.push(rate, Instruction::new(OpCode::Mov, best.base, d.base, 0));
            } else {
                self.push(
                    rate,
                    Instruction::new(OpCode::Min, best.base, best.base, d.base),
                );
            }
            self.release_to(mark);
        }
        Some(best)
    }

    fn emit_influence(&mut self, rate: Rate, point: Val, radius: Val, count: u32) -> Option<Val> {
        let sum = self.temp(1)?;
        let zero = self.constant(0.0);
        self.push(rate, Instruction::with_imm(OpCode::LoadK, sum.base, zero));
        for k in 0..count {
            let mark = self.next_temp;
            let off = self.emit_offset(rate, point, k)?;
            let d = self.temp(1)?;
            self.push(rate, Instruction::new(OpCode::Len3, d.base, off.base, 0));
            // A linear falloff that reaches zero at `radius` and never goes
            // negative: `max(0, 1 - d/r)`. Smooth enough for light, and three
            // instructions rather than the six a smoothstep would cost on every
            // one of `count` iterations.
            self.push(
                rate,
                Instruction::new(OpCode::Div, d.base, d.base, radius.base),
            );
            let one = self.constant(1.0);
            let t = self.temp(1)?;
            self.push(rate, Instruction::with_imm(OpCode::LoadK, t.base, one));
            self.push(rate, Instruction::new(OpCode::Sub, d.base, t.base, d.base));
            self.push(rate, Instruction::with_imm(OpCode::LoadK, t.base, zero));
            self.push(rate, Instruction::new(OpCode::Max, d.base, d.base, t.base));
            self.push(
                rate,
                Instruction::new(OpCode::Add, sum.base, sum.base, d.base),
            );
            self.release_to(mark);
        }
        Some(sum)
    }

    fn emit_field(&mut self, rate: Rate, point: Val, count: u32) -> Option<Val> {
        let sum = self.temp(3)?;
        let zero = self.constant(0.0);
        for lane in 0..3u8 {
            self.push(
                rate,
                Instruction::with_imm(OpCode::LoadK, sum.base + lane, zero),
            );
        }
        for k in 0..count {
            let mark = self.next_temp;
            let off = self.emit_offset(rate, point, k)?;
            // The contribution is the offset itself, so the sum is a vector
            // pointing away from where the elements are - which is what a flow
            // field wants. Unweighted: weighting by distance is what
            // `influence` is for, and doing it here too would make one accessor
            // two.
            for lane in 0..3u8 {
                self.push(
                    rate,
                    Instruction::new(
                        OpCode::Add,
                        sum.base + lane,
                        sum.base + lane,
                        off.base + lane,
                    ),
                );
            }
            self.release_to(mark);
        }
        Some(sum)
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
        // A sim body runs once per frame on one device, so rate says nothing
        // useful about where its instructions go.
        if self.in_sim {
            self.sim_body.push(ins);
            return;
        }
        if rate == Rate::Pixel {
            self.pixel.push(ins);
        } else {
            self.frame.push(ins);
        }
    }

    fn constant(&mut self, v: f64) -> u16 {
        // Into whichever program is being built. The two have separate pools,
        // and an immediate is an index into its own.
        if self.in_sim {
            return self.sim_builder.constant(to_q16(v));
        }
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
            if l.rate == Rate::Pixel || !l.used {
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
        // How long each layer-local `let` has to survive.
        //
        // Without this every one of them holds its register until the layer
        // ends, and a chain of them — `let a = ...`, `let b = f(a)`,
        // `let c = g(b)` — pins three registers while the colour expression,
        // which is where the peak actually is, needs only the last. Two of the
        // shipped examples sat at exactly 32 of 32 for this reason.
        let last_use = layer_let_last_use(layer);

        // The registers handed out to layer-local `let`s, in order, so dead ones
        // can be handed back from the top.
        let mut let_regs: Vec<(Val, usize)> = Vec::new();

        for (i, b) in layer.lets.iter().enumerate() {
            self.next_temp = self.temp_floor;
            let Some(v) = self.expr(Rate::Pixel, &b.value) else {
                return;
            };
            let width = v.width;

            // Anything whose last reader was this statement is now dead. Pop
            // from the top only: a stack allocator cannot free a hole, and the
            // chains this exists for die from the top anyway. Freeing before
            // the destination is allocated is what lets the destination land
            // where a dead value was.
            while let Some(&(reg, dead_after)) = let_regs.last() {
                if dead_after > i {
                    break;
                }
                self.temp_floor = reg.base;
                let_regs.pop();
            }

            self.next_temp = self.temp_floor;
            // Park it below the temporaries so the next expression does not
            // stamp on it.
            let Some(dst) = self.temp(width) else { return };
            // `move_into` copies ascending, so a destination below an
            // overlapping source is safe: every byte it overwrites has already
            // been read.
            self.move_into(Rate::Pixel, dst, v);
            self.temp_floor = self.next_temp;
            let_regs.push((dst, last_use[i]));
            self.bound.push((b.name.clone(), dst));
        }

        // Assigning a `state` inside a layer writes the per-pixel history
        // buffer, which is what makes trails and fire possible at all.
        for a in &layer.assigns {
            if a.target == "color" {
                continue;
            }
            let is_state = self.r.effect.states.iter().any(|st| st.name == a.target);
            if !is_state {
                continue;
            }
            self.next_temp = self.temp_floor;
            let Some(v) = self.expr(Rate::Pixel, &a.value) else {
                return;
            };
            // Write straight into the history registers rather than through
            // scratch. Two reasons: it costs three registers less on the
            // hottest path, and a later read of the state in the same frame
            // then sees the value just assigned, which is what an author who
            // writes `trail = ...` and then `color = trail` expects.
            for k in (0..3).rev() {
                let src = v.at(k);
                if R_PREV + k != src {
                    self.pixel
                        .push(Instruction::new(OpCode::Mov, R_PREV + k, src, 0));
                }
            }
            self.pixel
                .push(Instruction::new(OpCode::PrevWrite, R_PREV, 0, 0));
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
                // Inside a sim body, `p.pos` names one of the element's fields,
                // which is held whole in three registers rather than being a
                // lane of some larger value. Checked before the base is
                // evaluated, because evaluating it would look up `p` - which is
                // not a value and has no register of its own.
                if self.in_sim {
                    if let ExprKind::Ident(binding) = &base.kind {
                        let name = alloc::format!("{binding}.{field}");
                        if let Some(v) =
                            self.bound.iter().find(|(n, _)| n == &name).map(|(_, v)| *v)
                        {
                            return Some(v);
                        }
                    }
                }
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
            ExprKind::MethodCall { base, method, args } => {
                self.sim_accessor(rate, e, base, method, args)
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
                // `prev` is a colour, so three registers, matching the type
                // the grammar gives it.
                "prev" => Val {
                    base: R_PREV,
                    width: 3,
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
            SymbolKind::State => Some(Val {
                base: R_PREV,
                width: 3,
            }),
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
    /// The folded value of a parameter default.
    ///
    /// `resolve` has already rejected anything `const_value` cannot fold, so the
    /// fallback is unreachable through `compile`. It stays as a zero rather than
    /// a panic because a compiler that crashes on a malformed tree is worse than
    /// one that emits a dull colour, and the diagnostic has already been issued.
    fn const_expr_of(e: &Expr) -> Vec<f64> {
        crate::ast::const_value(e).unwrap_or_else(|| alloc::vec![0.0])
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

    /// Whether a destination at `base..base+width` can be written without
    /// destroying a source that has still to be read.
    ///
    /// A source is safe when it sits below the temporary floor (a builtin or a
    /// hoisted binding, which the destination can never occupy) or outside the
    /// destination range entirely. Anything else has to be copied somewhere
    /// fresh instead.
    ///
    /// This is checked rather than assumed. The tempting invariant - "component
    /// `k` was allocated at most at `mark + k`" - holds for a flat expression
    /// and breaks the moment an inlined function leaves its result higher up,
    /// which is exactly the case that produced silently wrong output.
    fn collapse_is_safe(&self, base: u8, width: u8, sources: &[Val]) -> bool {
        sources.iter().enumerate().all(|(k, v)| {
            let src = v.base;
            src < self.temp_floor || src == base + k as u8 || src >= base + width
        })
    }

    /// Gather values into a contiguous run, reusing their scratch when it is
    /// safe to do so.
    ///
    /// Moves run descending, so a source further up the range is read before
    /// anything below it is overwritten.
    fn pack(&mut self, rate: Rate, mark: u8, vals: &[Val]) -> Option<Val> {
        let width = vals.len() as u8;
        let dst = if self.collapse_is_safe(mark, width, vals) {
            self.release_to(mark);
            self.temp(width)?
        } else {
            self.temp(width)?
        };
        for k in (0..width).rev() {
            let src = vals[k as usize].base;
            if dst.base + k != src {
                self.push(rate, Instruction::new(OpCode::Mov, dst.base + k, src, 0));
            }
        }
        Some(dst)
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
            let mark = self.next_temp;
            let pos = self.expr(rate, &args[1])?;
            // PALETTE reads its position before writing the three components,
            // so the result may start exactly on top of the register that held
            // it - but not one or two above, which would clobber it mid-write.
            if self.collapse_is_safe(mark, 3, &[pos]) {
                self.release_to(mark);
            }
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
                return self.pack(rate, mark, &vals);
            }
            "hsv" => {
                let packed = self.pack(rate, mark, &vals)?;
                // HSV2RGB reads three registers and writes three; in place is
                // fine because the VM reads the whole source before writing.
                self.push(
                    rate,
                    Instruction::new(OpCode::Hsv2Rgb, packed.base, packed.base, 0),
                );
                return Some(packed);
            }
            "temp" => {
                self.release_to(mark);
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
                // NOISE3 reads three consecutive registers, so pack first. The
                // scalar result then reuses the first of them.
                let packed = self.pack(rate, mark, &vals)?;
                self.release_to(mark);
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
            // Vector forms. Each needs its operands as a contiguous run before
            // the instruction can read them, so they cannot go through the
            // scalar path below.
            "distance" => {
                let (a, b) = (vals[0], vals[1]);
                let width = a.width.max(b.width);
                let diff = self.temp(width)?;
                for k in 0..width {
                    self.push(
                        rate,
                        Instruction::new(OpCode::Sub, diff.base + k, a.at(k), b.at(k)),
                    );
                }
                let dst = self.temp(1)?;
                let ins = match width {
                    3 => Instruction::new(OpCode::Len3, dst.base, diff.base, 0),
                    2 => Instruction::new(OpCode::Len2, dst.base, diff.base, diff.base + 1),
                    // A scalar distance is the magnitude of the gap.
                    _ => Instruction::new(OpCode::Abs, dst.base, diff.base, 0),
                };
                self.push(rate, ins);
                return Some(dst);
            }
            "normalize" => {
                let v = vals[0];
                if v.width != 3 {
                    self.diags.push(Diagnostic::error(
                        e.span,
                        "`normalize` takes a vec3",
                        "build one with `vec3(x, y, z)`",
                    ));
                    return None;
                }
                let len = self.temp(1)?;
                self.push(rate, Instruction::new(OpCode::Len3, len.base, v.base, 0));
                let dst = self.temp(3)?;
                for k in 0..3 {
                    self.push(
                        rate,
                        Instruction::new(OpCode::Div, dst.base + k, v.at(k), len.base),
                    );
                }
                return Some(dst);
            }
            "cross" => {
                let (a, b) = (vals[0], vals[1]);
                if a.width != 3 || b.width != 3 {
                    self.diags.push(Diagnostic::error(
                        e.span,
                        "`cross` takes two vec3 values",
                        "build them with `vec3(x, y, z)`",
                    ));
                    return None;
                }
                let dst = self.temp(3)?;
                let t = self.temp(2)?;
                // Written out rather than looped: three components, and there is
                // no loop construct to lower it to anyway.
                for (k, (i, j)) in [(1u8, 2u8), (2, 0), (0, 1)].into_iter().enumerate() {
                    let k = k as u8;
                    self.push(
                        rate,
                        Instruction::new(OpCode::Mul, t.base, a.at(i), b.at(j)),
                    );
                    self.push(
                        rate,
                        Instruction::new(OpCode::Mul, t.base + 1, a.at(j), b.at(i)),
                    );
                    self.push(
                        rate,
                        Instruction::new(OpCode::Sub, dst.base + k, t.base, t.base + 1),
                    );
                }
                return Some(dst);
            }
            _ => {}
        }

        // Reusing the argument scratch for the destination saves a register on
        // every call, and is safe exactly when the destination cannot land on a
        // register the instruction has still to read.
        //
        // Two ways it can:
        //
        // A multi-instruction sequence allocates further temporaries after the
        // destination, and those land on arguments not yet consumed - `mod`
        // clobbered its own divisor that way and returned the dividend. Those
        // forms never reuse.
        //
        // The three-operand forms move one argument into the destination and
        // then read the other two. Reuse is sound when the destination lands on
        // the argument being moved, because the MOV is then a no-op; it is not
        // when that argument occupies no scratch of its own - a built-in like
        // `u`, or a `let` already in a register - because the destination lands
        // on the NEXT argument instead and the MOV overwrites it before the
        // operation reads it. That produced `clamp(u, 0.5, 1)` -> `clamp(u, u, 1)`,
        // `mix(u, 1, 0.5)` -> `mix(u, u, 0.5)` and `select(u, 1, 0)` returning
        // `u`. Each is right whenever the first argument is itself computed,
        // which is why the whole example corpus missed it.
        //
        // Naming the condition rather than the callees keeps `smoothstep` in the
        // same rule: it follows GLSL, so its value is the LAST argument, and the
        // check below excludes it for precisely the same reason.
        let multi_instruction =
            matches!(callee, "ceil" | "round" | "trunc" | "sign" | "mod" | "tan");
        // For the accumulator forms, the two registers the operation reads after
        // the destination has been written.
        // Indexed through `get` rather than `[]`: the resolver rejects a wrong
        // arity before this runs, but a panic here would be a compiler crash
        // rather than a diagnostic, and this is not the place to bet on that.
        let reads_after_move: Option<(u8, u8)> = match callee {
            "smoothstep" => Some((0usize, 1usize)),
            "clamp" | "mix" | "select" => Some((1usize, 2usize)),
            _ => None,
        }
        .and_then(|(i, j)| Some((vals.get(i)?.base, vals.get(j)?.base)));
        let reuses_scratch = vals.iter().all(|v| v.width == 1)
            && !multi_instruction
            && reads_after_move.is_none_or(|(a, b)| mark != a && mark != b);
        if reuses_scratch {
            self.release_to(mark);
        }
        let dst = self.temp(1)?;
        let ins = match callee {
            "abs" => Instruction::new(OpCode::Abs, dst.base, vals[0].base, 0),
            // No dedicated instruction for any of these: the instruction set is
            // frozen, and each is a couple of cheap ones. Capability grows in
            // the standard library, not in the VM.
            "ceil" => {
                // -floor(-x)
                self.push(
                    rate,
                    Instruction::new(OpCode::Neg, dst.base, vals[0].base, 0),
                );
                self.push(rate, Instruction::new(OpCode::Floor, dst.base, dst.base, 0));
                Instruction::new(OpCode::Neg, dst.base, dst.base, 0)
            }
            "round" => {
                let half = self.constant(0.5);
                let t = self.temp(1)?;
                self.push(rate, Instruction::with_imm(OpCode::LoadK, t.base, half));
                self.push(
                    rate,
                    Instruction::new(OpCode::Add, dst.base, vals[0].base, t.base),
                );
                Instruction::new(OpCode::Floor, dst.base, dst.base, 0)
            }
            "trunc" => {
                // Rounds toward zero, so it is floor of the magnitude with the
                // sign put back. Plain floor would take -0.5 to -1.
                let t = self.temp(2)?;
                self.push(rate, Instruction::new(OpCode::Abs, t.base, vals[0].base, 0));
                self.push(rate, Instruction::new(OpCode::Floor, t.base, t.base, 0));
                self.sign_into(rate, t.base + 1, vals[0].base)?;
                Instruction::new(OpCode::Mul, dst.base, t.base, t.base + 1)
            }
            "sign" => {
                self.sign_into(rate, dst.base, vals[0].base)?;
                Instruction::new(OpCode::Mov, dst.base, dst.base, 0)
            }
            "mod" => {
                let t = self.temp(2)?;
                self.push(
                    rate,
                    Instruction::new(OpCode::Div, t.base, vals[0].base, vals[1].base),
                );
                self.push(rate, Instruction::new(OpCode::Floor, t.base, t.base, 0));
                self.push(
                    rate,
                    Instruction::new(OpCode::Mul, t.base + 1, t.base, vals[1].base),
                );
                Instruction::new(OpCode::Sub, dst.base, vals[0].base, t.base + 1)
            }
            "tan" => {
                // sin/cos. It faults where cos is zero, which is exactly where
                // tan has no value - better than a huge number that looks like
                // an answer.
                let t = self.temp(2)?;
                self.push(rate, Instruction::new(OpCode::Sin, t.base, vals[0].base, 0));
                self.push(
                    rate,
                    Instruction::new(OpCode::Cos, t.base + 1, vals[0].base, 0),
                );
                Instruction::new(OpCode::Div, dst.base, t.base, t.base + 1)
            }
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
            // GLSL order: step(edge, x). The VM instruction takes the value
            // first, so the operands swap here rather than in the ISA - the
            // instruction set is frozen, the surface syntax is not.
            "step" => Instruction::new(OpCode::Step, dst.base, vals[1].base, vals[0].base),
            "noise1" => Instruction::new(OpCode::Noise1, dst.base, vals[0].base, 0),
            "noise2" => Instruction::new(OpCode::Noise2, dst.base, vals[0].base, vals[1].base),
            "length" => Instruction::new(OpCode::Len2, dst.base, vals[0].base, vals[1].base),
            "dot" => Instruction::new(OpCode::Mul, dst.base, vals[0].base, vals[1].base),
            "clamp" | "smoothstep" | "mix" | "select" => {
                // Three-operand forms where the destination is also the first
                // source, so one argument moves in before the operation.
                //
                // `smoothstep` follows GLSL - smoothstep(e0, e1, x) - so its
                // value is the LAST argument, not the first. Anyone reaching for
                // it has written a shader before, and a silently reversed
                // interpolation renders wrong rather than failing.
                let (into, a, b) = match callee {
                    "smoothstep" => (vals[2].base, vals[0].base, vals[1].base),
                    _ => (vals[0].base, vals[1].base, vals[2].base),
                };
                self.push(rate, Instruction::new(OpCode::Mov, dst.base, into, 0));
                let op = match callee {
                    "clamp" => OpCode::Clamp,
                    "smoothstep" => OpCode::SmoothStep,
                    "mix" => OpCode::Lerp,
                    _ => OpCode::Select,
                };
                Instruction::new(op, dst.base, a, b)
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

    /// `dst = sign(src)`: -1, 0 or +1.
    ///
    /// `(x > 0) - (x < 0)`, which is branch-free and gets zero right. A
    /// step-based version would call zero positive, and `sign(0) == 0` is what
    /// every other language means by it.
    fn sign_into(&mut self, rate: Rate, dst: u8, src: u8) -> Option<()> {
        let zero = self.constant(0.0);
        let t = self.temp(2)?;
        self.push(rate, Instruction::with_imm(OpCode::LoadK, t.base, zero));
        self.push(rate, Instruction::new(OpCode::Gt, dst, src, t.base));
        self.push(rate, Instruction::new(OpCode::Lt, t.base + 1, src, t.base));
        self.push(rate, Instruction::new(OpCode::Sub, dst, dst, t.base + 1));
        Some(())
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
        // `%` lowers to four instructions and needs scratch of its own, which
        // would land on the divisor it has still to read. Everything else here
        // is one instruction that reads its sources before writing.
        let single_instruction = op != BinOp::Rem;
        let dst = if width == 1 && a.width == 1 && b.width == 1 && single_instruction {
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
