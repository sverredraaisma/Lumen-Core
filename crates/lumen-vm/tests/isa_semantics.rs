//! One test per opcode: does the interpreter dispatch it to the right
//! operation, with the operands in the right order and the result in the right
//! register?
//!
//! The arithmetic itself is `q16`'s business and is tested there. What is *not*
//! tested there, and is the whole job of the dispatch loop, is the wiring: `SUB`
//! computing `c - b`, `ATAN2` receiving `(x, y)` instead of `(y, x)`, or
//! `SMOOTHSTEP` putting its answer in the edge register. Every one of those
//! produces plausible-looking light and none of them is visible in a percentage.
//!
//! So the expectation for each opcode is the corresponding `Q16` call, and every
//! input is deliberately asymmetric — swapping two operands must change the
//! answer, or the test proves nothing.
//!
//! The ISA is append-only: once an opcode ships, its meaning is frozen for every
//! program already compiled against it. These are the tests that freeze it.

use lumen_vm::isa::{Instruction, OpCode};
use lumen_vm::program::builder::ProgramBuilder;
use lumen_vm::program::{Program, Section, PALETTE_STOPS};
use lumen_vm::q16::Q16;
use lumen_vm::vm::{
    hsv_to_rgb, kelvin_to_rgb, rgb_to_hsv, Machine, NoUniforms, PixelInputs, Uniforms,
};

// General-purpose registers. 0..=15 carry the pixel inputs and the scratch slot,
// so tests stay above them and cannot be perturbed by input plumbing.
// Spaced three apart: several opcodes read or write a triple starting at their
// operand, so adjacent constants would make two "different" triples overlap and
// a test would silently assert against the wrong inputs.
const RA: u8 = 16;
const RB: u8 = 20;
const RC: u8 = 24;

/// Two values that differ, are not reciprocal, and are not each other's
/// negation, so an operand swap cannot coincidentally produce the same answer.
fn x() -> Q16 {
    Q16::from_ratio(5, 2) // 2.5
}
fn y() -> Q16 {
    Q16::from_ratio(3, 4) // 0.75
}

/// Assemble a pixel section, run it, and hand back the machine.
///
/// Operands arrive via `LOADK` rather than by poking registers, because that is
/// how a compiled program actually gets them here.
fn run(setup: &[(u8, Q16)], ins: &[Instruction]) -> Machine {
    let mut b = ProgramBuilder::new();
    for (reg, v) in setup {
        let k = b.constant(*v);
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, *reg, k),
        );
    }
    for i in ins {
        b.push(Section::Pixel, *i);
    }
    let bytes = b.build();
    let program = Program::parse(&bytes).expect("the builder must emit a parseable program");
    let mut m = Machine::new();
    m.run_pixel(&program, &PixelInputs::default(), &mut NoUniforms)
        .expect("a well-formed program must run without faulting");
    m
}

/// `a = op(b, c)` with `b` and `c` preloaded.
fn eval_bc(op: OpCode, b_val: Q16, c_val: Q16) -> Q16 {
    let m = run(
        &[(RB, b_val), (RC, c_val)],
        &[Instruction::new(op, RA, RB, RC)],
    );
    m.register(RA).expect("destination register")
}

/// `a = op(b)` with `b` preloaded.
fn eval_b(op: OpCode, b_val: Q16) -> Q16 {
    let m = run(&[(RB, b_val)], &[Instruction::new(op, RA, RB, 0)]);
    m.register(RA).expect("destination register")
}

/// `a = op(a, b, c)` — the accumulator forms, where `a` is read as well as
/// written. Getting this wrong is how `SMOOTHSTEP` once clobbered its own edge.
fn eval_abc(op: OpCode, a_val: Q16, b_val: Q16, c_val: Q16) -> Q16 {
    let m = run(
        &[(RA, a_val), (RB, b_val), (RC, c_val)],
        &[Instruction::new(op, RA, RB, RC)],
    );
    m.register(RA).expect("destination register")
}

// ---- 0x0_ core -------------------------------------------------------------

#[test]
fn mov_copies_b_into_a() {
    let m = run(&[(RB, x())], &[Instruction::new(OpCode::Mov, RA, RB, 0)]);
    assert_eq!(m.register(RA), Some(x()));
    assert_eq!(m.register(RB), Some(x()), "the source must be left alone");
}

#[test]
fn loadk_loads_the_constant_pool_entry() {
    let m = run(&[(RA, x())], &[]);
    assert_eq!(m.register(RA), Some(x()));
}

#[test]
fn nop_changes_nothing() {
    let m = run(&[(RA, x())], &[Instruction::new(OpCode::Nop, RA, RB, RC)]);
    assert_eq!(m.register(RA), Some(x()));
}

#[test]
fn add_and_mul_are_symmetric_sub_and_div_are_not() {
    assert_eq!(eval_bc(OpCode::Add, x(), y()), x().add(y()));
    assert_eq!(eval_bc(OpCode::Mul, x(), y()), x().mul(y()));

    // The ordered pair. `b - c`, not `c - b`.
    assert_eq!(eval_bc(OpCode::Sub, x(), y()), x().sub(y()));
    assert_ne!(
        eval_bc(OpCode::Sub, x(), y()),
        y().sub(x()),
        "SUB must not be commutative"
    );

    assert_eq!(
        eval_bc(OpCode::Div, x(), y()),
        x().div(y()).expect("2.5/0.75")
    );
    assert_ne!(
        eval_bc(OpCode::Div, x(), y()),
        y().div(x()).expect("0.75/2.5"),
        "DIV must not be commutative"
    );
}

#[test]
fn madd_multiplies_the_accumulator_and_adds_c() {
    // `a = a * b + c`, with the product kept at full width before the add.
    assert_eq!(
        eval_abc(OpCode::Madd, x(), y(), Q16::ONE),
        x().madd(y(), Q16::ONE)
    );
}

#[test]
fn neg_and_abs_round_trip_a_negative() {
    let neg = eval_b(OpCode::Neg, x());
    assert_eq!(neg, x().neg());
    assert_eq!(eval_b(OpCode::Abs, neg), x(), "abs must undo neg");
}

#[test]
fn min_and_max_pick_the_right_end() {
    assert_eq!(eval_bc(OpCode::Min, x(), y()), y(), "0.75 < 2.5");
    assert_eq!(eval_bc(OpCode::Max, x(), y()), x(), "2.5 > 0.75");
    // Order must not matter for these two, unlike SUB.
    assert_eq!(eval_bc(OpCode::Min, y(), x()), y());
    assert_eq!(eval_bc(OpCode::Max, y(), x()), x());
}

#[test]
fn clamp_reads_its_value_from_the_destination() {
    let lo = Q16::ONE;
    let hi = Q16::from_int(2);
    assert_eq!(
        eval_abc(OpCode::Clamp, x(), lo, hi),
        hi,
        "2.5 clamps down to 2"
    );
    assert_eq!(
        eval_abc(OpCode::Clamp, y(), lo, hi),
        lo,
        "0.75 clamps up to 1"
    );
    assert_eq!(
        eval_abc(OpCode::Clamp, Q16::from_ratio(3, 2), lo, hi),
        Q16::from_ratio(3, 2),
        "a value already inside the range is untouched"
    );
}

#[test]
fn floor_and_fract_split_a_value() {
    let v = Q16::from_ratio(5, 2);
    let f = eval_b(OpCode::Floor, v);
    let r = eval_b(OpCode::Fract, v);
    assert_eq!(f, v.floor());
    assert_eq!(r, v.fract());
    assert_eq!(f.add(r), v, "floor + fract must reconstruct the input");
}

#[test]
fn floor_and_fract_agree_on_negatives_too() {
    // The sign convention is the part people get wrong, so pin it.
    let v = Q16::from_ratio(-5, 2);
    assert_eq!(eval_b(OpCode::Floor, v), v.floor());
    assert_eq!(eval_b(OpCode::Fract, v), v.fract());
    assert_eq!(eval_b(OpCode::Floor, v).add(eval_b(OpCode::Fract, v)), v);
}

// ---- 0x1_ transcendental ---------------------------------------------------

#[test]
fn trig_dispatches_to_the_matching_q16_call() {
    let a = Q16::from_ratio(1, 3);
    assert_eq!(eval_b(OpCode::Sin, a), a.sin());
    assert_eq!(eval_b(OpCode::Cos, a), a.cos());
    assert_eq!(eval_b(OpCode::SinTurns, a), a.sin_turns());
    assert_eq!(eval_b(OpCode::CosTurns, a), a.cos_turns());
}

#[test]
fn sin_and_cos_are_not_swapped() {
    // At a quarter turn the two differ maximally, so a swapped pair is loud.
    let quarter = Q16::from_ratio(1, 4);
    assert_ne!(
        eval_b(OpCode::SinTurns, quarter),
        eval_b(OpCode::CosTurns, quarter)
    );
}

#[test]
fn atan2_takes_y_then_x() {
    let r = eval_bc(OpCode::Atan2, x(), y());
    assert_eq!(r, Q16::atan2(x(), y()));
    assert_ne!(
        r,
        Q16::atan2(y(), x()),
        "ATAN2's operands are ordered (y, x) and must not be swapped"
    );
}

#[test]
fn sqrt_pow_exp_and_the_logs_dispatch_correctly() {
    let a = Q16::from_int(4);
    assert_eq!(eval_b(OpCode::Sqrt, a), a.sqrt().expect("sqrt(4)"));
    assert_eq!(eval_b(OpCode::Exp, Q16::ONE), Q16::ONE.exp());
    assert_eq!(eval_b(OpCode::Log, a), a.ln().expect("ln(4)"));
    assert_eq!(eval_b(OpCode::Log2, a), a.log2().expect("log2(4)"));

    let r = eval_bc(OpCode::Pow, x(), y());
    assert_eq!(r, x().pow(y()).expect("2.5^0.75"));
    assert_ne!(
        r,
        y().pow(x()).expect("0.75^2.5"),
        "POW's base and exponent must not be swapped"
    );
}

// ---- 0x2_ noise ------------------------------------------------------------

#[test]
fn noise_is_deterministic_and_dimension_sensitive() {
    let a = Q16::from_ratio(7, 4);
    let b = Q16::from_ratio(9, 4);

    let n1 = eval_b(OpCode::Noise1, a);
    assert_eq!(
        n1,
        eval_b(OpCode::Noise1, a),
        "noise must be a pure function"
    );

    let n2 = eval_bc(OpCode::Noise2, a, b);
    assert_eq!(n2, eval_bc(OpCode::Noise2, a, b));
    assert_ne!(
        n2,
        eval_bc(OpCode::Noise2, b, a),
        "2D noise must not be symmetric in its arguments"
    );
}

#[test]
fn noise3_reads_three_consecutive_registers() {
    // `NOISE3 a, b` reads b, b+1, b+2 — an off-by-one here silently samples a
    // neighbouring register and the field looks merely "different", not wrong.
    let m = run(
        &[
            (RB, Q16::from_ratio(1, 4)),
            (RB + 1, Q16::from_ratio(2, 4)),
            (RB + 2, Q16::from_ratio(3, 4)),
        ],
        &[Instruction::new(OpCode::Noise3, RA, RB, 0)],
    );
    let got = m.register(RA).expect("destination");

    // Changing the third component must change the result, which proves all
    // three registers were actually read.
    let m2 = run(
        &[
            (RB, Q16::from_ratio(1, 4)),
            (RB + 1, Q16::from_ratio(2, 4)),
            (RB + 2, Q16::from_ratio(3, 4).neg()),
        ],
        &[Instruction::new(OpCode::Noise3, RA, RB, 0)],
    );
    assert_ne!(got, m2.register(RA).expect("destination"));
}

// ---- 0x3_ compare and select ----------------------------------------------

#[test]
fn comparisons_yield_one_or_zero_and_are_correctly_oriented() {
    // b op c, with b = 2.5 and c = 0.75.
    assert_eq!(eval_bc(OpCode::Lt, x(), y()), Q16::ZERO);
    assert_eq!(eval_bc(OpCode::Lt, y(), x()), Q16::ONE);
    assert_eq!(eval_bc(OpCode::Gt, x(), y()), Q16::ONE);
    assert_eq!(eval_bc(OpCode::Gt, y(), x()), Q16::ZERO);
}

#[test]
fn the_inclusive_comparisons_differ_from_the_strict_ones_only_at_equality() {
    assert_eq!(eval_bc(OpCode::Lt, x(), x()), Q16::ZERO);
    assert_eq!(eval_bc(OpCode::Le, x(), x()), Q16::ONE);
    assert_eq!(eval_bc(OpCode::Gt, x(), x()), Q16::ZERO);
    assert_eq!(eval_bc(OpCode::Ge, x(), x()), Q16::ONE);
    assert_eq!(eval_bc(OpCode::Eq, x(), x()), Q16::ONE);
    assert_eq!(eval_bc(OpCode::Eq, x(), y()), Q16::ZERO);
}

#[test]
fn select_reads_its_condition_from_the_destination_register() {
    // `a = (a != 0) ? b : c` — branchless, which is the point.
    assert_eq!(eval_abc(OpCode::Select, Q16::ONE, x(), y()), x());
    assert_eq!(eval_abc(OpCode::Select, Q16::ZERO, x(), y()), y());
    assert_eq!(
        eval_abc(OpCode::Select, Q16::ONE.neg(), x(), y()),
        x(),
        "any non-zero condition selects b, not just positive ones"
    );
}

#[test]
fn step_compares_against_its_edge_in_the_right_direction() {
    assert_eq!(eval_bc(OpCode::Step, x(), y()), x().step(y()));
    assert_eq!(
        eval_bc(OpCode::Step, y(), x()),
        y().step(x()),
        "value and edge must not be interchangeable"
    );
    assert_ne!(
        eval_bc(OpCode::Step, x(), y()),
        eval_bc(OpCode::Step, y(), x())
    );
}

#[test]
fn smoothstep_takes_its_value_from_the_destination_and_its_edges_from_b_and_c() {
    // This is the exact shape that once made the destination clobber `e0`.
    let e0 = Q16::ZERO;
    let e1 = Q16::from_int(2);
    let v = Q16::ONE;
    assert_eq!(
        eval_abc(OpCode::SmoothStep, v, e0, e1),
        v.smoothstep(e0, e1)
    );
    assert_eq!(
        eval_abc(OpCode::SmoothStep, e0, e0, e1),
        Q16::ZERO,
        "at the lower edge smoothstep is 0"
    );
    assert_eq!(
        eval_abc(OpCode::SmoothStep, e1, e0, e1),
        Q16::ONE,
        "at the upper edge smoothstep is 1"
    );
}

#[test]
fn lerp_interpolates_from_the_destination_towards_b() {
    // `a = lerp(a, b, t = c)`.
    let from = Q16::ZERO;
    let to = Q16::from_int(4);
    assert_eq!(
        eval_abc(OpCode::Lerp, from, to, Q16::ZERO),
        from,
        "t=0 is `from`"
    );
    assert_eq!(
        eval_abc(OpCode::Lerp, from, to, Q16::ONE),
        to,
        "t=1 is `to`"
    );
    assert_eq!(
        eval_abc(OpCode::Lerp, from, to, Q16::HALF),
        from.lerp(to, Q16::HALF)
    );
}

// ---- 0x4_ space ------------------------------------------------------------

#[test]
fn len2_is_the_hypotenuse() {
    assert_eq!(eval_bc(OpCode::Len2, x(), y()), Q16::len2(x(), y()));
    // 3-4-5, exactly representable, so this pins the value and not just the call.
    assert_eq!(
        eval_bc(OpCode::Len2, Q16::from_int(3), Q16::from_int(4)),
        Q16::from_int(5)
    );
}

#[test]
fn len3_reads_three_consecutive_registers() {
    let m = run(
        &[
            (RB, Q16::from_int(2)),
            (RB + 1, Q16::from_int(3)),
            (RB + 2, Q16::from_int(6)),
        ],
        &[Instruction::new(OpCode::Len3, RA, RB, 0)],
    );
    // 2-3-6-7 is a Pythagorean quadruple, so this is exact in Q16.
    assert_eq!(m.register(RA), Some(Q16::from_int(7)));
}

#[test]
fn dot3_pairs_two_register_triples() {
    let m = run(
        &[
            (RB, Q16::from_int(1)),
            (RB + 1, Q16::from_int(2)),
            (RB + 2, Q16::from_int(3)),
            (RC, Q16::from_int(4)),
            (RC + 1, Q16::from_int(5)),
            (RC + 2, Q16::from_int(6)),
        ],
        &[Instruction::new(OpCode::Dot3, RA, RB, RC)],
    );
    // 1*4 + 2*5 + 3*6 = 32.
    assert_eq!(m.register(RA), Some(Q16::from_int(32)));
}

// ---- 0x5_ colour -----------------------------------------------------------

#[test]
fn hsv2rgb_writes_three_consecutive_registers() {
    let h = Q16::from_ratio(1, 3);
    let s = Q16::ONE;
    let v = Q16::ONE;
    let m = run(
        &[(RB, h), (RB + 1, s), (RB + 2, v)],
        &[Instruction::new(OpCode::Hsv2Rgb, RA, RB, 0)],
    );
    let expected = hsv_to_rgb(h, s, v);
    assert_eq!(m.register(RA), Some(expected[0]));
    assert_eq!(m.register(RA + 1), Some(expected[1]));
    assert_eq!(m.register(RA + 2), Some(expected[2]));
}

#[test]
fn rgb2hsv_writes_three_consecutive_registers() {
    let r = Q16::ONE;
    let g = Q16::HALF;
    let b = Q16::ZERO;
    let m = run(
        &[(RB, r), (RB + 1, g), (RB + 2, b)],
        &[Instruction::new(OpCode::Rgb2Hsv, RA, RB, 0)],
    );
    let expected = rgb_to_hsv(r, g, b);
    assert_eq!(m.register(RA), Some(expected[0]));
    assert_eq!(m.register(RA + 1), Some(expected[1]));
    assert_eq!(m.register(RA + 2), Some(expected[2]));
}

#[test]
fn temp2rgb_matches_the_kelvin_table() {
    let k = Q16::from_int(3000);
    let m = run(&[(RB, k)], &[Instruction::new(OpCode::Temp2Rgb, RA, RB, 0)]);
    let expected = kelvin_to_rgb(k);
    assert_eq!(m.register(RA), Some(expected[0]));
    assert_eq!(m.register(RA + 1), Some(expected[1]));
    assert_eq!(m.register(RA + 2), Some(expected[2]));
}

#[test]
fn palette_samples_the_stop_table() {
    let stops: [(Q16, Q16, Q16); PALETTE_STOPS] = core::array::from_fn(|i| {
        let t = Q16::from_ratio(i as i32, (PALETTE_STOPS - 1) as i32);
        (t, Q16::ZERO, Q16::ONE.sub(t))
    });

    let mut b = ProgramBuilder::new();
    let slot = b.palette(&stops);
    let k = b.constant(Q16::ZERO);
    b.push(Section::Pixel, Instruction::with_imm(OpCode::LoadK, RB, k));
    b.push(
        Section::Pixel,
        Instruction::new(OpCode::Palette, RA, RB, slot),
    );
    let bytes = b.build();
    let program = Program::parse(&bytes).expect("parse");
    let mut m = Machine::new();
    m.run_pixel(&program, &PixelInputs::default(), &mut NoUniforms)
        .expect("run");

    let expected = program
        .palette_sample(slot, Q16::ZERO)
        .expect("the palette slot must exist");
    assert_eq!(m.register(RA), Some(expected.0));
    assert_eq!(m.register(RA + 1), Some(expected.1));
    assert_eq!(m.register(RA + 2), Some(expected.2));
}

// ---- uniforms and history --------------------------------------------------

/// A channel source that answers with the slot index, so a test can tell which
/// slot was actually read.
struct SlotEcho;

impl Uniforms for SlotEcho {
    fn channel(&self, slot: u8, offset: u8) -> Q16 {
        Q16::from_int(slot as i16 * 10 + offset as i16)
    }
}

#[test]
fn chread_passes_the_slot_and_offset_through() {
    let mut b = ProgramBuilder::new();
    let slot = b.channel(0x1234);
    b.push(
        Section::Pixel,
        Instruction::new(OpCode::ChRead, RA, slot, 3),
    );
    let bytes = b.build();
    let program = Program::parse(&bytes).expect("parse");
    let mut m = Machine::new();
    m.run_pixel(&program, &PixelInputs::default(), &mut SlotEcho)
        .expect("run");
    assert_eq!(
        m.register(RA),
        Some(Q16::from_int(slot as i16 * 10 + 3)),
        "CHREAD must forward both the slot and the byte offset"
    );
}
