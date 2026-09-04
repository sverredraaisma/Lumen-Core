//! Emitter tests that pin register allocation and the exact bytecode.
//!
//! `tests/compile.rs` deliberately checks the emitter by *running* it, because
//! an instruction listing breaks on every harmless change to allocation. This
//! file does the opposite on purpose, for the handful of forms where the
//! allocation *is* the behaviour under test.
//!
//! The emitter's failures have all had one shape: a register reused while
//! something still had to be read out of it. A rendered pixel hides that
//! whenever the clobbered value happens to equal the one that replaced it -
//! which is exactly why `clamp(u, 0.5, 1)` quietly returned `u` and every
//! example in the corpus still looked right. So each test here asserts the
//! listing *and*, where a wrong register would change the answer, renders a
//! pixel that only comes out right if the allocation is right.

use lumen_lang::compile;
use lumen_vm::isa::{Instruction, OpCode};
use lumen_vm::program::{Program, Section};
use lumen_vm::q16::Q16;
use lumen_vm::vm::{Machine, NoUniforms, PixelInputs, PixelOutput};

/// An instruction as `(op, a, b, c)`, which is what the assertions read best.
type Step = (OpCode, u8, u8, u8);

fn compiled(src: &str) -> lumen_lang::emit::Compiled {
    let (out, diags) = compile(src);
    out.unwrap_or_else(|| panic!("compile failed:\n{}", diags.render(src)))
}

fn steps_of(ins: &[Instruction]) -> Vec<Step> {
    ins.iter().map(|i| (i.op, i.a, i.b, i.c)).collect()
}

/// The pixel section, decoded.
fn pixel(src: &str) -> Vec<Instruction> {
    let bytes = compiled(src).bytecode;
    let program = Program::parse(&bytes).expect("the emitter produced an invalid program");
    (0..program.section_len(Section::Pixel))
        .map(|i| program.instruction(Section::Pixel, i).unwrap())
        .collect()
}

/// Registers the report says the program needs live at its widest point.
fn registers(src: &str) -> u8 {
    compiled(src).report.registers_used
}

fn render(src: &str, inputs: PixelInputs) -> PixelOutput {
    let out = compiled(src);
    let program = Program::parse(&out.bytecode).unwrap();
    let mut m = Machine::new();
    m.run_frame_at(&program, Q16::ZERO, Q16::ZERO, &mut NoUniforms)
        .unwrap();
    m.run_pixel(&program, &inputs, &mut NoUniforms).unwrap()
}

/// Render at `u` and take the red channel, which is where these tests park the
/// value under test.
fn red_at(src: &str, u: Q16) -> Q16 {
    match render(
        src,
        PixelInputs {
            u,
            ..Default::default()
        },
    ) {
        PixelOutput::Rgb { r, .. } => r,
        other => panic!("expected an RGB emit, got {other:?}"),
    }
}

fn errors(src: &str) -> Vec<String> {
    let (_, diags) = compile(src);
    diags.errors().map(|d| d.message.clone()).collect()
}

fn wrap(body: &str) -> String {
    format!("lumen 1\neffect \"x\" {{\n{body}\n}}\n")
}

// The register layout every listing below is written against: inputs occupy
// 0..15, the accumulator takes the first three permanents, and scratch starts
// immediately above it.
//
// `ACCUM` follows `lumen_vm::vm::R_SCRATCH_NO_DT`, because none of these
// fixtures reads `dt`. An effect that does gives up register 15 to hold it and
// every listing here shifts up by one — which is the whole point of the flag in
// the program header: only the programs that ask for `dt` pay for it.
const R_U: u8 = 8;
const R_PREV: u8 = 12;
const R_DT: u8 = 15;
const ACCUM: u8 = 15;
const SCRATCH: u8 = 18;

// ---- The three-operand arm, which is where the clobber lived ---------------

#[test]
fn clamp_keeps_its_bounds_when_the_value_needs_no_scratch() {
    // Regression. `clamp` emits a MOV and then the operation, so it is a
    // two-instruction form and must not reuse the argument scratch. It used to,
    // and when the first argument occupied no scratch of its own - a built-in
    // like `u`, or a `let` already in a register - the destination landed on the
    // SECOND argument and the MOV overwrote the low bound before CLAMP read it.
    // `clamp(u, 0.5, 1)` compiled to `clamp(u, u, 1)`, which is `u`: right for
    // every `u` inside the bounds, and silently wrong at the one place a clamp
    // exists to handle.
    let src = wrap("  layer l {\n    color = rgb(clamp(u, 0.5, 1), 0, 0)\n  }");
    let ins = pixel(&src);
    assert_eq!(
        steps_of(&ins)[..4],
        [
            // The bounds are loaded first and must survive the MOV.
            (OpCode::LoadK, SCRATCH, 0, 0),
            (OpCode::LoadK, SCRATCH + 1, 1, 0),
            (OpCode::Mov, SCRATCH + 2, R_U, 0),
            (OpCode::Clamp, SCRATCH + 2, SCRATCH, SCRATCH + 1),
        ],
        "{ins:#?}"
    );
    // And it clamps: below the low bound the answer is the bound, not the input.
    assert_eq!(red_at(&src, Q16::ZERO), Q16::HALF);
    assert_eq!(red_at(&src, Q16::ONE), Q16::ONE);
}

#[test]
fn mix_interpolates_towards_its_second_argument() {
    // The same clobber as `clamp`: the MOV landed on `b`, so `mix(u, 1, 0.5)`
    // became `mix(u, u, 0.5)`, which is `u`.
    let src = wrap("  layer l {\n    color = rgb(mix(u, 1, 0.5), 0, 0)\n  }");
    assert_eq!(
        steps_of(&pixel(&src))[..4],
        [
            (OpCode::LoadK, SCRATCH, 0, 0),
            (OpCode::LoadK, SCRATCH + 1, 1, 0),
            (OpCode::Mov, SCRATCH + 2, R_U, 0),
            (OpCode::Lerp, SCRATCH + 2, SCRATCH, SCRATCH + 1),
        ]
    );
    // Halfway from 0 to 1 is a half, not zero.
    assert_eq!(red_at(&src, Q16::ZERO), Q16::HALF);
}

#[test]
fn select_returns_its_branches_not_its_condition() {
    // And again: `select(u, 1, 0)` returned `u` rather than 1.
    let src = wrap("  layer l {\n    color = rgb(select(u, 1, 0), 0, 0)\n  }");
    assert_eq!(
        steps_of(&pixel(&src))[..4],
        [
            (OpCode::LoadK, SCRATCH, 0, 0),
            (OpCode::LoadK, SCRATCH + 1, 1, 0),
            (OpCode::Mov, SCRATCH + 2, R_U, 0),
            (OpCode::Select, SCRATCH + 2, SCRATCH, SCRATCH + 1),
        ]
    );
    assert_eq!(red_at(&src, Q16::from_ratio(1, 4)), Q16::ONE);
    assert_eq!(red_at(&src, Q16::ZERO), Q16::ZERO);
}

#[test]
fn smoothstep_still_takes_its_value_from_the_last_argument() {
    // `smoothstep` shares the arm the three above sit in, and follows GLSL:
    // smoothstep(e0, e1, x). The MOV brings in the LAST argument, not the first.
    let src = wrap("  layer l {\n    color = rgb(smoothstep(0, 1, u), 0, 0)\n  }");
    assert_eq!(
        steps_of(&pixel(&src))[..4],
        [
            (OpCode::LoadK, SCRATCH, 0, 0),
            (OpCode::LoadK, SCRATCH + 1, 1, 0),
            (OpCode::Mov, SCRATCH + 2, R_U, 0),
            (OpCode::SmoothStep, SCRATCH + 2, SCRATCH, SCRATCH + 1),
        ]
    );
    // Rising, not falling.
    assert!(red_at(&src, Q16::from_ratio(1, 4)) < red_at(&src, Q16::from_ratio(3, 4)));
}

// ---- Packing and the collapse decision -------------------------------------

#[test]
fn packing_collapses_onto_the_mark_when_no_source_is_in_the_way() {
    // Two parameters and a literal. The first parameter already sits exactly on
    // the mark and every other source sits above the three registers the pack
    // needs, so the pack folds back onto the mark - and the moves run descending,
    // reading each source before the register below it is overwritten.
    let src = wrap(
        "  param tint : color = #ff8000\n  param v : vec3 = vec3(1, -2, 3)\n  layer l {\n    color = rgb(tint.r, v.y, 0)\n  }",
    );
    let ins = pixel(&src);
    assert_eq!(
        steps_of(&ins),
        [
            // tint, three components.
            (OpCode::LoadK, 18, 0, 0),
            (OpCode::LoadK, 19, 1, 0),
            (OpCode::LoadK, 20, 2, 0),
            // v, three components.
            (OpCode::LoadK, 21, 0, 0),
            (OpCode::LoadK, 22, 3, 0),
            (OpCode::LoadK, 23, 4, 0),
            // The literal zero.
            (OpCode::LoadK, 24, 2, 0),
            // Collapsed back onto 18..20, descending, so r24 and r22 are read
            // before r20 and r19 are written over.
            (OpCode::Mov, 20, 24, 0),
            (OpCode::Mov, 19, 22, 0),
            (OpCode::Mov, 15, 18, 0),
            (OpCode::Mov, 16, 19, 0),
            (OpCode::Mov, 17, 20, 0),
            (OpCode::EmitRgb, 15, 16, 17),
        ],
        "{ins:#?}"
    );
    assert_eq!(registers(&src), 25);
}

#[test]
fn packing_refuses_to_collapse_onto_a_source_it_would_clobber() {
    // `-vec3(x, y, z)`: the negation lands above the mark, so collapsing the
    // outer pack back onto the mark would overwrite the vector while its later
    // components still had to be read. It allocates fresh instead - the case
    // that produced silently wrong output when the collapse was assumed safe
    // rather than checked.
    let src = wrap(
        "  layer l {
    let a = -vec3(x, y, z)
    color = rgb(a.x, a.y, a.z)
  }",
    );
    let ins = pixel(&src);
    assert_eq!(
        steps_of(&ins),
        [
            // vec3(x, y, z), packed descending onto the mark.
            (OpCode::Mov, 20, 2, 0),
            (OpCode::Mov, 19, 1, 0),
            (OpCode::Mov, 18, 0, 0),
            // One NEG per component, into fresh registers.
            (OpCode::Neg, 21, 18, 0),
            (OpCode::Neg, 22, 19, 0),
            (OpCode::Neg, 23, 20, 0),
            // The binding comes home to the bottom of its own scratch, which
            // is where the vector it was built from used to be. Everything the
            // expression borrowed above it is dead once the value has moved.
            (OpCode::Mov, 18, 21, 0),
            (OpCode::Mov, 19, 22, 0),
            (OpCode::Mov, 20, 23, 0),
            // The rgb pack cannot collapse onto 18: that is where `a.x` lives.
            (OpCode::Mov, 23, 20, 0),
            (OpCode::Mov, 22, 19, 0),
            (OpCode::Mov, 21, 18, 0),
            (OpCode::Mov, 15, 21, 0),
            (OpCode::Mov, 16, 22, 0),
            (OpCode::Mov, 17, 23, 0),
            (OpCode::EmitRgb, 15, 16, 17),
        ],
        "{ins:#?}"
    );
    // Still the widest shape the language can express in one binding, and it now
    // leaves eight registers spare rather than two.
    assert_eq!(registers(&src), 24);
}

// ---- Forms that need a contiguous run --------------------------------------

#[test]
fn length_of_three_components_packs_them_and_uses_len3() {
    // LEN3 reads three consecutive registers, so the components move into a run
    // first. The scalar result then sits above them, and the surrounding pack
    // collapses back down onto the run nothing needs any more.
    let src = wrap("  layer l {\n    color = rgb(length(x, y, z), 0, 0)\n  }");
    assert_eq!(
        steps_of(&pixel(&src)),
        [
            (OpCode::Mov, 18, 0, 0),
            (OpCode::Mov, 19, 1, 0),
            (OpCode::Mov, 20, 2, 0),
            (OpCode::Len3, 21, 18, 0),
            (OpCode::LoadK, 22, 0, 0),
            (OpCode::LoadK, 23, 0, 0),
            (OpCode::Mov, 20, 23, 0),
            (OpCode::Mov, 19, 22, 0),
            (OpCode::Mov, 18, 21, 0),
            (OpCode::Mov, 15, 18, 0),
            (OpCode::Mov, 16, 19, 0),
            (OpCode::Mov, 17, 20, 0),
            (OpCode::EmitRgb, 15, 16, 17),
        ]
    );
}

#[test]
fn noise3_reuses_the_run_it_packed() {
    // NOISE3 reads three consecutive registers and writes one. The scalar result
    // takes the first of them back, so the call costs three registers, not four.
    let src = wrap("  layer l {\n    color = rgb(noise3(x, y, z), 0, 0)\n  }");
    assert_eq!(
        steps_of(&pixel(&src)),
        [
            (OpCode::Mov, 20, 2, 0),
            (OpCode::Mov, 19, 1, 0),
            (OpCode::Mov, 18, 0, 0),
            (OpCode::Noise3, 18, 18, 0),
            (OpCode::LoadK, 19, 0, 0),
            (OpCode::LoadK, 20, 0, 0),
            (OpCode::Mov, 15, 18, 0),
            (OpCode::Mov, 16, 19, 0),
            (OpCode::Mov, 17, 20, 0),
            (OpCode::EmitRgb, 15, 16, 17),
        ]
    );
    assert_eq!(registers(&src), 21);
}

#[test]
fn hsv_converts_in_place_over_the_run_it_packed() {
    // HSV2RGB reads three registers and writes three. In place is safe because
    // the VM reads the whole source before writing, and it saves three registers
    // on the per-pixel path.
    let src = wrap("  layer l {\n    color = hsv(u, 1, 1)\n  }");
    assert_eq!(
        steps_of(&pixel(&src)),
        [
            (OpCode::LoadK, 18, 0, 0),
            (OpCode::LoadK, 19, 0, 0),
            // `u` sits below the mark but the saturation does not, so the pack
            // cannot collapse and takes 20..22 instead.
            (OpCode::Mov, 22, 19, 0),
            (OpCode::Mov, 21, 18, 0),
            (OpCode::Mov, 20, R_U, 0),
            (OpCode::Hsv2Rgb, 20, 20, 0),
            (OpCode::Mov, 15, 20, 0),
            (OpCode::Mov, 16, 21, 0),
            (OpCode::Mov, 17, 22, 0),
            (OpCode::EmitRgb, 15, 16, 17),
        ]
    );
}

#[test]
fn temp_scales_the_blackbody_colour_by_its_intensity() {
    // TEMP2RGB writes three registers from one, and the intensity multiplies
    // each of them afterwards.
    let src = wrap("  layer l {\n    color = temp(3000, u)\n  }");
    assert_eq!(
        steps_of(&pixel(&src)),
        [
            (OpCode::LoadK, 18, 0, 0),
            (OpCode::Temp2Rgb, 18, 18, 0),
            (OpCode::Mul, 18, 18, R_U),
            (OpCode::Mul, 19, 19, R_U),
            (OpCode::Mul, 20, 20, R_U),
            (OpCode::Mov, 15, 18, 0),
            (OpCode::Mov, 16, 19, 0),
            (OpCode::Mov, 17, 20, 0),
            (OpCode::EmitRgb, 15, 16, 17),
        ]
    );
    // Intensity zero is black however hot the source.
    match render(&src, PixelInputs::default()) {
        PixelOutput::Rgb { r, g, b } => {
            assert_eq!((r, g, b), (Q16::ZERO, Q16::ZERO, Q16::ZERO));
        }
        other => panic!("{other:?}"),
    }
}

// ---- Inputs that need no load at all ---------------------------------------

#[test]
fn pos_reads_the_position_registers_without_loading_them() {
    // `pos` is already three consecutive input registers, so reading it costs
    // moves and no loads. That is the whole reason the built-ins are laid out
    // the way they are.
    let src = wrap("  layer l {\n    color = rgb(pos.x, pos.y, pos.z)\n  }");
    let ins = pixel(&src);
    assert!(
        ins.iter().all(|i| i.op != OpCode::LoadK),
        "reading `pos` should not touch the constant pool: {ins:#?}"
    );
    assert_eq!(
        steps_of(&ins),
        [
            (OpCode::Mov, 20, 2, 0),
            (OpCode::Mov, 19, 1, 0),
            (OpCode::Mov, 18, 0, 0),
            (OpCode::Mov, 15, 18, 0),
            (OpCode::Mov, 16, 19, 0),
            (OpCode::Mov, 17, 20, 0),
            (OpCode::EmitRgb, 15, 16, 17),
        ]
    );
}

#[test]
fn mapq_reads_as_zero_until_a_device_is_mapped() {
    // There is no register for mapping quality yet. Zero is the honest answer
    // for a device that has not been mapped, and it has to be a real load rather
    // than whatever a register happened to hold.
    let src = wrap("  layer l {\n    color = rgb(mapq, 0, 0)\n  }");
    assert_eq!(
        steps_of(&pixel(&src)),
        [
            (OpCode::LoadK, 18, 0, 0),
            (OpCode::LoadK, 19, 0, 0),
            (OpCode::LoadK, 20, 0, 0),
            (OpCode::Mov, 15, 18, 0),
            (OpCode::Mov, 16, 19, 0),
            (OpCode::Mov, 17, 20, 0),
            (OpCode::EmitRgb, 15, 16, 17),
        ]
    );
    assert_eq!(red_at(&src, Q16::ONE), Q16::ZERO);
}

#[test]
fn every_mention_of_a_channel_shares_one_slot() {
    // CHREAD names a *slot*, and the slot table is what lets a host repoint a
    // program at a different channel id without recompiling. A slot per read
    // rather than per channel meant two mentions of `bass` produced two slots
    // both naming channel 0: a host had to find and rewrite every one of them,
    // and the slot index is a `u8`, so an effect reading one channel 256 times
    // would have wrapped.
    let src = wrap(
        "  channel bass : audio_bands hold 200 default 0
  layer l {
    color = rgb(bass, bass, 0)
  }",
    );
    assert_eq!(
        steps_of(&pixel(&src))[..3],
        [
            (OpCode::ChRead, 18, 0, 0),
            (OpCode::ChRead, 19, 0, 0),
            (OpCode::LoadK, 20, 0, 0),
        ],
        "both reads name slot 0"
    );
    let bytes = compiled(&src).bytecode;
    let program = Program::parse(&bytes).unwrap();
    assert_eq!(program.channel_count(), 1, "one channel, one slot");
    assert_eq!(program.channel_id(0), Some(0));
}

// ---- Operators -------------------------------------------------------------

#[test]
fn negation_emits_one_instruction_and_no_constant() {
    // `-x` is a NEG, not a multiply by a loaded -1.
    let src = wrap("  layer l {\n    color = rgb(-u, 0, 0)\n  }");
    assert_eq!(
        steps_of(&pixel(&src))[..1],
        [(OpCode::Neg, SCRATCH, R_U, 0)]
    );
    assert_eq!(registers(&src), 21);
}

#[test]
fn logical_not_is_a_comparison_against_zero() {
    // Branch-free, because the pixel section has no branches to spare.
    let src = wrap("  layer l {\n    color = rgb(!u, 0, 0)\n  }");
    assert_eq!(
        steps_of(&pixel(&src))[..2],
        [
            (OpCode::LoadK, SCRATCH + 1, 0, 0),
            (OpCode::Eq, SCRATCH, R_U, SCRATCH + 1),
        ]
    );
    assert_eq!(red_at(&src, Q16::ZERO), Q16::ONE);
    assert_eq!(red_at(&src, Q16::ONE), Q16::ZERO);
}

#[test]
fn not_equal_is_one_minus_equal() {
    // There is no NE instruction and the ISA is frozen, so it is `1 - (a == b)`,
    // which stays branch-free.
    let src = wrap("  layer l {\n    color = rgb(u != 0.5, 0, 0)\n  }");
    assert_eq!(
        steps_of(&pixel(&src))[..4],
        [
            (OpCode::LoadK, SCRATCH, 0, 0),
            (OpCode::Eq, SCRATCH, R_U, SCRATCH),
            (OpCode::LoadK, SCRATCH + 1, 1, 0),
            (OpCode::Sub, SCRATCH, SCRATCH + 1, SCRATCH),
        ]
    );
    assert_eq!(red_at(&src, Q16::HALF), Q16::ZERO);
    assert_eq!(red_at(&src, Q16::ONE), Q16::ONE);
}

#[test]
fn each_comparison_gets_its_own_opcode() {
    let src = wrap("  layer l {\n    color = rgb(u < 0.5, u <= 0.5, u > 0.5)\n  }");
    let ops: Vec<OpCode> = pixel(&src).iter().map(|i| i.op).collect();
    assert_eq!(
        ops[..6],
        [
            OpCode::LoadK,
            OpCode::Lt,
            OpCode::LoadK,
            OpCode::Le,
            OpCode::LoadK,
            OpCode::Gt,
        ]
    );
    let src = wrap("  layer l {\n    color = rgb(u >= 0.5, u == 0.5, 0)\n  }");
    let ops: Vec<OpCode> = pixel(&src).iter().map(|i| i.op).collect();
    assert_eq!(ops[1], OpCode::Ge);
    assert_eq!(ops[3], OpCode::Eq);
}

#[test]
fn and_is_min_and_or_is_max() {
    // Both operands are already 0 or 1, so the comparisons do the work and no
    // branch is needed.
    let src = wrap(
        "  layer l {\n    color = rgb((u > 0.1) && (u < 0.9), (u > 0.1) || (u < 0.9), 0)\n  }",
    );
    let ops: Vec<OpCode> = pixel(&src).iter().map(|i| i.op).collect();
    assert_eq!(ops[4], OpCode::Min);
    assert_eq!(ops[9], OpCode::Max);
    // Below both edges, `and` is false and `or` is still true.
    match render(
        &src,
        PixelInputs {
            u: Q16::ZERO,
            ..Default::default()
        },
    ) {
        PixelOutput::Rgb { r, g, .. } => {
            assert_eq!(r, Q16::ZERO);
            assert_eq!(g, Q16::ONE);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_remainder_operator_keeps_the_divisor_it_still_has_to_read() {
    // `%` lowers to four instructions and needs scratch of its own, which is why
    // it is excluded from the reuse. It clobbered its own divisor once and
    // returned the dividend.
    let src = wrap("  layer l {\n    color = rgb(u % 0.25, 0, 0)\n  }");
    assert_eq!(
        steps_of(&pixel(&src))[..5],
        [
            (OpCode::LoadK, 18, 0, 0),
            // The result register is 19; the divisor stays untouched at 18.
            (OpCode::Div, 20, R_U, 18),
            (OpCode::Floor, 20, 20, 0),
            (OpCode::Mul, 21, 20, 18),
            (OpCode::Sub, 19, R_U, 21),
        ]
    );
    // 0.75 % 0.25 is zero; a clobbered divisor would return the dividend.
    assert_eq!(red_at(&src, Q16::from_ratio(3, 4)), Q16::ZERO);
}

// ---- Blending --------------------------------------------------------------

#[test]
fn a_second_normal_layer_moves_rather_than_composites() {
    // The first layer moves straight into the accumulator; a later one with the
    // same blend goes through the blend path and comes out as three moves.
    let src =
        wrap("  layer a { color = rgb(1, 0, 0) }\n  layer b blend normal { color = rgb(0, 1, 0) }");
    let ins = pixel(&src);
    assert_eq!(
        steps_of(&ins)[9..],
        [
            (OpCode::Mov, ACCUM, 18, 0),
            (OpCode::Mov, ACCUM + 1, 19, 0),
            (OpCode::Mov, ACCUM + 2, 20, 0),
            (OpCode::EmitRgb, ACCUM, ACCUM + 1, ACCUM + 2),
        ],
        "{ins:#?}"
    );
    // Green on top wins outright.
    match render(&src, PixelInputs::default()) {
        PixelOutput::Rgb { r, g, .. } => {
            assert_eq!(r, Q16::ZERO);
            assert_eq!(g, Q16::ONE);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn additive_and_multiplicative_blends_are_one_instruction_per_channel() {
    // The cheap blends, and the reason a layered effect is affordable at all:
    // three instructions on top of whatever the layer itself cost.
    let src =
        wrap("  layer a { color = rgb(1, 0, 0) }\n  layer b blend add { color = rgb(0, 1, 0) }");
    assert_eq!(
        steps_of(&pixel(&src))[9..12],
        [
            (OpCode::Add, 15, 15, 18),
            (OpCode::Add, 16, 16, 19),
            (OpCode::Add, 17, 17, 20),
        ]
    );
    match render(&src, PixelInputs::default()) {
        PixelOutput::Rgb { r, g, b } => {
            assert_eq!((r, g, b), (Q16::ONE, Q16::ONE, Q16::ZERO));
        }
        other => panic!("{other:?}"),
    }

    let src =
        wrap("  layer a { color = rgb(1, 0, 0) }\n  layer b blend min { color = rgb(0.5, 1, 1) }");
    assert_eq!(
        steps_of(&pixel(&src))[9..12],
        [
            (OpCode::Min, 15, 15, 18),
            (OpCode::Min, 16, 16, 19),
            (OpCode::Min, 17, 17, 20),
        ]
    );

    let src =
        wrap("  layer a { color = rgb(1, 0, 0) }\n  layer b blend max { color = rgb(0.5, 1, 0) }");
    assert_eq!(
        steps_of(&pixel(&src))[9..12],
        [
            (OpCode::Max, 15, 15, 18),
            (OpCode::Max, 16, 16, 19),
            (OpCode::Max, 17, 17, 20),
        ]
    );

    let src = wrap(
        "  layer a { color = rgb(1, 1, 0) }\n  layer b blend multiply { color = rgb(0.5, 0, 0) }",
    );
    assert_eq!(
        steps_of(&pixel(&src))[9..12],
        [
            (OpCode::Mul, 15, 15, 18),
            (OpCode::Mul, 16, 16, 19),
            (OpCode::Mul, 17, 17, 20),
        ]
    );
    match render(&src, PixelInputs::default()) {
        PixelOutput::Rgb { r, g, .. } => {
            assert_eq!(r, Q16::HALF);
            assert_eq!(g, Q16::ZERO);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn difference_is_a_subtract_and_an_absolute_value_per_channel() {
    let src = wrap(
        "  layer a { color = rgb(1, 0, 0) }\n  layer b blend difference { color = rgb(0, 1, 0) }",
    );
    assert_eq!(
        steps_of(&pixel(&src))[9..15],
        [
            (OpCode::Sub, 15, 15, 18),
            (OpCode::Abs, 15, 15, 0),
            (OpCode::Sub, 16, 16, 19),
            (OpCode::Abs, 16, 16, 0),
            (OpCode::Sub, 17, 17, 20),
            (OpCode::Abs, 17, 17, 0),
        ]
    );
    // |1 - 0| and |0 - 1| are both one.
    match render(&src, PixelInputs::default()) {
        PixelOutput::Rgb { r, g, b } => {
            assert_eq!((r, g, b), (Q16::ONE, Q16::ONE, Q16::ZERO));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn screen_is_one_minus_the_product_of_the_complements() {
    let src =
        wrap("  layer a { color = rgb(1, 0, 0) }\n  layer b blend screen { color = rgb(0, 1, 0) }");
    // One channel's worth: load 1, take both complements, multiply, subtract.
    assert_eq!(
        steps_of(&pixel(&src))[9..14],
        [
            (OpCode::LoadK, 21, 0, 0),
            (OpCode::Sub, 22, 21, 15),
            (OpCode::Sub, 23, 21, 18),
            (OpCode::Mul, 22, 22, 23),
            (OpCode::Sub, 15, 21, 22),
        ]
    );
    match render(&src, PixelInputs::default()) {
        PixelOutput::Rgb { r, g, b } => {
            assert_eq!((r, g, b), (Q16::ONE, Q16::ONE, Q16::ZERO));
        }
        other => panic!("{other:?}"),
    }
    // Each channel takes its own scratch and none of it is handed back between
    // channels, so a screen blend is the widest thing an effect can do. This is
    // the guard that says it still fits in the register file - barely.
    assert_eq!(registers(&src), 30);
}

#[test]
fn overlay_is_a_clamped_double_product() {
    let src = wrap(
        "  layer a { color = rgb(1, 0, 0) }\n  layer b blend overlay { color = rgb(0, 1, 0) }",
    );
    assert_eq!(
        steps_of(&pixel(&src))[9..15],
        [
            (OpCode::LoadK, 21, 2, 0),
            (OpCode::Mul, 15, 15, 18),
            (OpCode::Mul, 15, 15, 21),
            (OpCode::LoadK, 21, 1, 0),
            (OpCode::LoadK, 22, 0, 0),
            (OpCode::Clamp, 15, 21, 22),
        ]
    );
    // 2 * 1 * 0 is zero, and the clamp keeps it in range.
    match render(&src, PixelInputs::default()) {
        PixelOutput::Rgb { r, g, b } => {
            assert_eq!((r, g, b), (Q16::ZERO, Q16::ZERO, Q16::ZERO));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(registers(&src), 27);
}

// ---- Constants and palettes ------------------------------------------------

#[test]
fn a_constant_too_big_for_q16_saturates_rather_than_wrapping() {
    // Q16.16 tops out around 32768. A wrap would turn a huge brightness into a
    // huge negative one, which reads as black and looks like a different bug.
    let src = wrap("  layer l {\n    color = rgb(100000, 0, 0)\n  }");
    let bytes = compiled(&src).bytecode;
    let program = Program::parse(&bytes).unwrap();
    assert_eq!(program.constant(0), Some(Q16(i32::MAX)));

    let src =
        wrap("  param p : float = -100000 range -200000..0\n  layer l { color = rgb(p, 0, 0) }");
    let bytes = compiled(&src).bytecode;
    let program = Program::parse(&bytes).unwrap();
    assert_eq!(program.constant(0), Some(Q16(i32::MIN)));
}

#[test]
fn a_palette_holds_its_end_colour_beyond_the_outermost_stop() {
    // The baked table is evenly spaced over 0..1, so a palette whose last stop
    // is at 0.5 has to hold that colour across the whole upper half rather than
    // running off the end of the stop list.
    let src = "lumen 1\npalette p {\n  space linear_rgb\n  0 #000000\n  0.5 #ffffff\n}\neffect \"x\" {\n  layer l { color = palette(p, u) }\n}\n";
    let bytes = compiled(src).bytecode;
    let program = Program::parse(&bytes).unwrap();
    assert_eq!(
        program.palette_sample(0, Q16::ZERO),
        Some((Q16::ZERO, Q16::ZERO, Q16::ZERO))
    );
    for slot in [8, 12, 15] {
        assert_eq!(
            program.palette_sample(0, Q16::from_ratio(slot, 16)),
            Some((Q16::ONE, Q16::ONE, Q16::ONE)),
            "slot {slot} should hold the last stop"
        );
    }
}

#[test]
fn a_parameter_default_is_folded_component_wise() {
    // A parameter is a compile-time constant: it changes between activations,
    // never within one. A negated literal inside a constructor keeps its sign.
    let src =
        wrap("  param v : vec3 = vec3(1, -2, 3)\n  layer l {\n    color = rgb(v.x, v.y, v.z)\n  }");
    let bytes = compiled(&src).bytecode;
    let program = Program::parse(&bytes).unwrap();
    let loads: Vec<Q16> = pixel(&src)
        .iter()
        .filter(|i| i.op == OpCode::LoadK)
        .map(|i| program.constant(i.bc()).unwrap())
        .collect();
    assert_eq!(
        loads[..3],
        [Q16::ONE, Q16(-2 * 65536), Q16(3 * 65536)],
        "the default did not fold to its three components"
    );
    // A parameter is materialised afresh at every mention rather than bound to a
    // register once, so three field reads cost three full loads of the default.
    assert_eq!(loads.len(), 9);
}

// ---- Warts pinned so a change to them is deliberate -------------------------

#[test]
fn a_parameter_default_that_is_not_a_constant_is_refused() {
    // Constant folding stops at literals, negated literals and `rgb`/`vec3` of
    // those. An arithmetic default used to become zero silently: the slider sat
    // in its declared range while the effect rendered as though the parameter
    // were nothing, with no diagnostic to explain it.
    let src = wrap(
        "  param p : float = 0.5 * 2 range 0..4
  layer l { color = rgb(p, 0, 0) }",
    );
    let errs = errors(&src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(
        errs[0].contains("the default for `p` is not a constant"),
        "{errs:?}"
    );
}

#[test]
fn the_defaults_the_emitter_can_fold_are_still_accepted() {
    // The other half of the same rule: rejecting too much would be worse than
    // the wart it replaced.
    for default in ["1.5", "-0.25", "#204080", "rgb(1, 0, 0)", "vec3(0, 1, 0)"] {
        let ty = if default.starts_with('#') || default.starts_with("rgb") {
            "color"
        } else if default.starts_with("vec3") {
            "vec3"
        } else {
            "float"
        };
        let range = if ty == "float" { " range -1..2" } else { "" };
        let src = wrap(&format!(
            "  param p : {ty} = {default}{range}
  layer l {{ color = rgb(0, 0, 0) }}"
        ));
        assert!(errors(&src).is_empty(), "{default}: {:?}", errors(&src));
    }
}

#[test]
fn a_field_the_register_model_has_no_room_for_reads_the_first_component() {
    // Colours occupy three registers; alpha has none. `prev.a` therefore reads
    // the red channel rather than failing. Pinned because it is silent, not
    // because it is right: the two spellings compile to the same bytes.
    let alpha = wrap("  layer l {\n    color = rgb(prev.a, 0, 0)\n  }");
    let red = wrap("  layer l {\n    color = rgb(prev.r, 0, 0)\n  }");
    assert!(errors(&alpha).is_empty(), "{:?}", errors(&alpha));
    assert!(
        steps_of(&pixel(&alpha)).contains(&(OpCode::Mov, 20, R_PREV, 0)),
        "{:#?}",
        pixel(&alpha)
    );
    assert_eq!(compiled(&alpha).bytecode, compiled(&red).bytecode);
}

#[test]
fn an_assignment_to_a_name_that_is_neither_color_nor_state_is_refused() {
    // The emitter had nowhere to put it and skipped it in silence, so `other = 1`
    // compiled to byte-identical bytecode to writing nothing at all. "An unknown
    // construct is an error, never skipped" is the rule; this was the exception.
    let src = wrap(
        "  layer l {
    other = 1
    color = rgb(1, 0, 0)
  }",
    );
    let errs = errors(&src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(
        errs[0].contains("nothing named `other` can be assigned here"),
        "{errs:?}"
    );
}

#[test]
fn assigning_a_declared_state_in_a_layer_is_still_accepted() {
    let src = wrap(
        "  state heat : color = rgb(0, 0, 0)
  layer l {
    heat = rgb(1, 0, 0)
    color = heat
  }",
    );
    assert!(errors(&src).is_empty(), "{:?}", errors(&src));
}

// ---- Diagnostics -----------------------------------------------------------

#[test]
fn a_string_is_not_a_value() {
    let src = wrap("  let a = \"hi\"\n  layer l { color = rgb(a, 0, 0) }");
    let e = errors(&src);
    assert!(e.contains(&"a string is not a value".to_string()), "{e:?}");
    // And the binding it failed to produce is reported at its use, rather than
    // the compiler carrying on with a register that was never written.
    assert!(e.contains(&"`a` cannot be used here".to_string()), "{e:?}");
}

#[test]
fn an_accessor_on_a_sim_channel_is_refused_for_want_of_a_count() {
    // Accessors lower now, unrolled over the element count - so they need one.
    // A `sim<..>` channel names a record type and carries no count, so the
    // bound an accumulation would unroll against is not knowable from it.
    // Refused by name rather than lowered against a guess.
    let src = wrap(
        "  channel swarm : sim<flock>
  layer l { let a = swarm.nearest(vec3(x, y, z))
    color = rgb(a, 0, 0) }",
    );
    assert!(
        errors(&src).contains(&"`swarm` does not declare how many elements it has".to_string()),
        "{:?}",
        errors(&src)
    );
}

#[test]
fn a_palette_name_is_not_a_value_on_its_own() {
    let src = "lumen 1\npalette p {\n  0 #000000\n  1 #ffffff\n}\neffect \"x\" {\n  layer l { color = rgb(p, 0, 0) }\n}\n";
    assert_eq!(errors(src), ["`p` is a palette, not a value"]);
}

#[test]
fn the_first_argument_to_palette_must_be_a_name() {
    // Palettes are referenced by identifier, never by string: a string would
    // have to be resolved at runtime, which the VM has no way to do.
    let src = "lumen 1\npalette p {\n  0 #000000\n  1 #ffffff\n}\neffect \"x\" {\n  layer l { color = palette(\"p\", u) }\n}\n";
    assert_eq!(
        errors(src),
        ["the first argument to `palette` must be a palette name"]
    );
}

#[test]
fn a_palette_with_no_stops_is_refused_by_name() {
    let src = "lumen 1\npalette p {\n}\neffect \"x\" {\n  layer l { color = palette(p, u) }\n}\n";
    assert_eq!(errors(src), ["palette `p` has no stops"]);
}

#[test]
fn a_palette_stop_must_be_a_colour_literal() {
    let src = "lumen 1\npalette p {\n  0 1\n  1 #ffffff\n}\neffect \"x\" {\n  layer l { color = palette(p, u) }\n}\n";
    assert_eq!(errors(src), ["a palette stop must be a colour literal"]);
}

#[test]
fn functions_nested_deeper_than_the_inline_cap_are_refused() {
    // Functions are always inlined, so a chain deeper than the cap would inline
    // forever. This chain is acyclic, so the resolver's cycle check does not
    // catch it: the cap is the only thing between the compiler and its own
    // stack, and a diagnostic beats a stack overflow.
    let mut src = String::from("lumen 1\neffect \"x\" {\n");
    for i in 0..20 {
        src.push_str(&format!(
            "  fn f{i}(v : float) -> float {{\n    return f{}(v)\n  }}\n",
            i + 1
        ));
    }
    src.push_str("  fn f20(v : float) -> float {\n    return v\n  }\n");
    src.push_str("  layer l { color = rgb(f0(u), 0, 0) }\n}\n");
    assert_eq!(
        errors(&src),
        ["functions nest too deeply, or call each other in a cycle"]
    );
}

#[test]
fn a_failure_inside_a_function_body_stops_the_inline() {
    // The body's own `let` cannot be emitted, so the inline abandons the return
    // expression rather than binding a register that was never written.
    let src = wrap(
        "  fn bad(v : float) -> float {\n    let s = \"nope\"\n    return v + s\n  }\n  layer l { color = rgb(bad(u), 0, 0) }",
    );
    assert_eq!(errors(&src), ["a string is not a value"]);
}

// ---- Running out of registers ----------------------------------------------

#[test]
fn running_out_of_scratch_is_reported_once_and_not_as_a_flood() {
    // Every subsequent allocation would report the same thing, and a page of
    // identical errors buries the one line that says what to do about it.
    let mut src = String::from("lumen 1\neffect \"x\" {\n  layer l {\n");
    for i in 0..12 {
        src.push_str(&format!("    let a{i} = vec3(x + {i}, y, z)\n"));
    }
    src.push_str("    color = rgb(a0.x, a1.y, a2.z)\n  }\n}\n");
    assert_eq!(
        errors(&src),
        ["the effect needs more registers than the VM has"]
    );
}

#[test]
fn running_out_of_permanent_registers_is_reported_too() {
    // Effect-level `let`s at pixel rate take permanent registers, because
    // several layers may read them. Enough of them exhausts the file before a
    // single temporary is allocated.
    let mut src = String::from("lumen 1\neffect \"x\" {\n");
    for i in 0..8 {
        src.push_str(&format!("  let a{i} = vec3(x + {i}, y, z)\n"));
    }
    let reads: Vec<String> = (0..8).map(|i| format!("a{i}.x")).collect();
    src.push_str(&format!(
        "  layer l {{ color = rgb({}, 0, 0) }}\n}}\n",
        reads.join(" + ")
    ));
    let e = errors(&src);
    assert_eq!(e[0], "the effect needs more registers than the VM has");
    // The bindings that never got a register are then reported at their use,
    // which is how the author learns which one tipped it over.
    assert_eq!(e[1], "`a3` cannot be used here");
}

#[test]
fn a_layer_opacity_that_cannot_be_allocated_fails_the_whole_compile() {
    // Opacity is evaluated after the layer's colour, so it is the last thing to
    // run out of registers - and the compile has to fail rather than emit a
    // layer that composites unscaled.
    //
    // Both bindings are read by the colour, so both are still live when the
    // opacity runs. Tuned so the colour itself still fits and only the opacity
    // does not.
    let src = "lumen 1
effect \"x\" {
  layer l opacity length(x, y, z) {
    let a = vec3(x, y, z)
    let b = vec3(x + 1, y, z)
    color = rgb(a.x + b.x, y, z)
  }
}
";
    assert_eq!(
        errors(src),
        ["the effect needs more registers than the VM has"]
    );
}

#[test]
fn a_state_assignment_that_cannot_be_allocated_fails_the_whole_compile() {
    // A `state` write goes straight into the history registers, but the
    // expression feeding it still needs scratch like anything else.
    //
    // Every binding is read by the state expression, so none can be handed back
    // early. That is load-bearing: a binding nobody reads is dead the moment it
    // is written and costs nothing, so a version of this test with unread
    // bindings would fit comfortably and prove nothing.
    let mut src = String::from(
        "lumen 1
effect \"x\" {
  state h : color = rgb(0, 0, 0)
  layer l {
",
    );
    for i in 0..4 {
        src.push_str(&format!(
            "    let q{i} = vec3(x + {i}, y, z)
"
        ));
    }
    src.push_str(
        "    h = vec3(q0.x + q1.x + q2.x + q3.x, y, z)
    color = prev
  }
}
",
    );
    assert_eq!(
        errors(&src),
        ["the effect needs more registers than the VM has"]
    );
}

#[test]
fn the_accumulator_itself_can_be_what_does_not_fit() {
    // The accumulator is reserved after the hoisted bindings and before the
    // temporary floor is set. Enough hoisted bindings and there is no room left
    // for the three registers the compositor needs - which has to be the same
    // diagnostic, not a panic on the way past it.
    let mut src = String::from("lumen 1\neffect \"x\" {\n");
    for i in 0..5 {
        src.push_str(&format!("  let f{i} = vec3(t + {i}, t, t)\n"));
    }
    let reads: Vec<String> = (0..5).map(|i| format!("f{i}.x")).collect();
    src.push_str(&format!(
        "  layer l {{ color = rgb({}, 0, 0) }}\n}}\n",
        reads.join(" + ")
    ));
    assert_eq!(
        errors(&src)[0],
        "the effect needs more registers than the VM has"
    );
}

/// An effect with `count` bindings that each simply read `base`, all summed
/// into the colour so every one of them counts as used and takes a register.
///
/// Every binding is a bare read, so none of them needs scratch of its own. That
/// is deliberate: it makes the *reservation* the thing that runs out, rather
/// than an expression running out of room one binding earlier and reporting
/// from somewhere else.
fn saturating_lets(base: &str, count: usize) -> String {
    let mut src = String::from("lumen 1\neffect \"x\" {\n");
    for i in 0..count {
        src.push_str(&format!("  let a{i} = {base}\n"));
    }
    let reads: Vec<String> = (0..count).map(|i| format!("a{i}")).collect();
    src.push_str(&format!(
        "  layer l {{ color = rgb({}, 0, 0) }}\n}}\n",
        reads.join(" + ")
    ));
    src
}

#[test]
fn a_pixel_rate_binding_that_gets_no_register_is_reported_at_the_effect() {
    // An effect-level `let` that could not be hoisted still needs a register
    // that survives the whole pixel, because several layers may read it. Once
    // the permanents are gone there is nowhere to put the next one, and that has
    // to be the register diagnostic rather than a binding quietly missing.
    let src = saturating_lets("x", 15);
    assert_eq!(
        errors(&src)[0],
        "the effect needs more registers than the VM has"
    );
    // Exactly one diagnostic: the emitter stops at the first binding it cannot
    // place rather than reporting every remaining one.
    assert_eq!(errors(&src).len(), 1);
}

#[test]
fn a_hoisted_binding_that_does_not_fit_is_reported_at_the_effect() {
    // Frame-rate bindings keep their register for the whole frame, so they come
    // out of the same file the accumulator and the temporaries share. One more
    // than fits is an error at the effect, with advice about hoisting - not a
    // silently truncated program.
    let src = saturating_lets("t", 18);
    let (out, diags) = compile(&src);
    assert!(out.is_none());
    let d = diags.errors().next().unwrap();
    assert_eq!(d.message, "the effect needs more registers than the VM has");
    assert!(d.help.contains("hoisted"), "{}", d.help);
    assert_eq!(diags.errors().count(), 1, "one diagnostic, not a flood");
}

// ---- Determinism -----------------------------------------------------------

#[test]
fn every_form_in_this_file_compiles_to_the_same_bytes_twice() {
    // Nothing here may iterate a hash map and emit in iteration order. Checked
    // over the widest programs in the file, because that is where a stray map
    // would show up first.
    for body in [
        "  layer l { color = rgb(clamp(u, 0.5, 1), mix(u, 1, 0.5), select(u, 1, 0)) }",
        "  layer a { color = rgb(1, 0, 0) }\n  layer b blend screen { color = rgb(0, 1, 0) }",
        "  param tint : color = #ff8000\n  layer l { color = rgb(tint.r, tint.g, tint.b) }",
        "  channel bass : audio_bands hold 200 default 0\n  layer l { color = hsv(bass, 1, 1) }",
    ] {
        let src = wrap(body);
        assert_eq!(compiled(&src).bytecode, compiled(&src).bytecode, "{body}");
    }
}

// ---- Sim accessors ---------------------------------------------------------
//
// Accessors are the *green* half of `sim`: they run per pixel on every device
// against its own coordinates, reading state broadcast on a `sim<..>` channel.
// A device does not have to run the simulation to use them, which is why they
// can be typed - and eventually emitted - independently of the `sim` block that
// is still refused.

fn sim_errors(body: &str) -> Vec<String> {
    errors(&wrap(&format!(
        "  channel swarm : sim<flock>\n  layer l {{ {body} }}"
    )))
}

#[test]
fn a_sim_is_not_a_number() {
    // It typed as `float` until now, so this compiled and would have emitted
    // whatever register the channel happened to occupy.
    let es = sim_errors("color = rgb(swarm, 0, 0)");
    assert!(
        es.iter()
            .any(|e| e.contains("cannot be used") || e.contains("sim") || e.contains("float")),
        "{es:?}"
    );
}

#[test]
fn an_unknown_accessor_names_the_ones_that_exist() {
    let es = sim_errors("color = rgb(swarm.nonsense(u), 0, 0)");
    assert!(
        es.iter().any(|e| e == "a sim has no accessor `nonsense`"),
        "{es:?}"
    );
}

#[test]
fn count_is_a_field_and_not_a_method() {
    // The mistake is easy to make - the other three are calls - so the message
    // says which it is rather than that the name is unknown.
    let es = sim_errors("color = rgb(swarm.count(1), 0, 0)");
    assert!(
        es.iter().any(|e| e == "a sim has no accessor `count`"),
        "{es:?}"
    );
}

#[test]
fn an_unknown_field_says_what_a_sim_has() {
    let es = sim_errors("color = rgb(swarm.size, 0, 0)");
    assert!(
        es.iter().any(|e| e == "a sim has no field `size`"),
        "{es:?}"
    );
}

#[test]
fn an_accessor_checks_how_many_arguments_it_takes() {
    let es = sim_errors("color = rgb(swarm.nearest(vec3(x,y,z), 1), 0, 0)");
    assert!(
        es.iter()
            .any(|e| e.contains("`nearest` takes 1 arguments, but 2 were given")),
        "{es:?}"
    );
}

#[test]
fn a_point_argument_must_be_three_wide() {
    // The mistake that would miscompile rather than fail: a scalar where a
    // position belongs reads one lane and silently measures against the wrong
    // point. Checked on width because width is what the emitter works in - a
    // `color` here is three lanes and lowers correctly.
    let es = sim_errors("color = rgb(swarm.nearest(u), 0, 0)");
    assert!(
        es.iter()
            .any(|e| e.contains("argument 1 of `nearest` is `vec3`")),
        "{es:?}"
    );
}

#[test]
fn a_well_formed_accessor_against_a_declared_sim_compiles() {
    // The sim block is what exposes the accessors, and it is what declares the
    // count they unroll over. Still refused overall, because the sim's *body*
    // has no lowering - but the accessor itself no longer contributes an error.
    let es = errors(&wrap(
        "  sim swarm(count = 3) {
    foreach p in swarm { p.pos = p.pos }
  }
  layer l { let a = swarm.nearest(vec3(x, y, z))
    color = rgb(a, 0, 0) }",
    ));
    assert!(
        es.iter().all(|e| e.contains("cannot be compiled yet")),
        "the accessor itself was rejected: {es:?}"
    );
}

#[test]
fn methods_belong_to_sims_and_nothing_else() {
    let es = errors(&wrap("  layer l { color = rgb(u.influence(u), 0, 0) }"));
    assert!(
        es.iter().any(|e| e.contains("has no method `influence`")),
        "{es:?}"
    );
}

// ---- Sim blocks ------------------------------------------------------------
//
// The body is checked and still not lowered. Splitting it that way is worth
// having on its own: what an author writes is understood and reported against
// precisely, and what is missing is code generation rather than comprehension.

fn sim_block_errors(header: &str, body: &str) -> Vec<String> {
    errors(&wrap(&format!(
        "  sim {header} {{\n{body}\n  }}\n  layer l {{ color = rgb(0, 0, 0) }}"
    )))
}

/// Errors other than the standing "not implemented", which every sim gets.
fn sim_complaints(header: &str, body: &str) -> Vec<String> {
    sim_block_errors(header, body)
        .into_iter()
        .filter(|e| !e.contains("cannot be compiled yet"))
        .collect()
}

#[test]
fn a_well_formed_sim_is_refused_only_for_its_body() {
    let es = sim_complaints(
        "swarm(count = 64)",
        "    let drag = 0.99\n    foreach p in swarm {\n      p.vel = p.vel * drag\n      p.pos = p.pos + p.vel\n    }",
    );
    assert!(es.is_empty(), "{es:?}");
}

#[test]
fn a_sim_needs_a_count() {
    // It sizes an array in a profile with no dynamic allocation, and it is what
    // makes a per-pixel accessor costable before the effect ships.
    let es = sim_complaints("swarm()", "    let a = 1");
    assert!(es.iter().any(|e| e == "a sim needs a `count`"), "{es:?}");
}

#[test]
fn a_count_must_be_a_constant() {
    let es = sim_complaints("swarm(count = u)", "    let a = 1");
    assert!(
        es.iter().any(|e| e == "`count` must be a constant"),
        "{es:?}"
    );
}

#[test]
fn a_sim_with_no_elements_is_refused() {
    let es = sim_complaints("swarm(count = 0)", "    let a = 1");
    assert!(
        es.iter().any(|e| e == "`count` must be at least 1"),
        "{es:?}"
    );
}

#[test]
fn a_sim_iterates_its_own_elements() {
    // The sim's name denotes its elements, which is what the accessor table
    // already implies - `swarm.count` is the element count. A name meaning one
    // thing to a loop and another to an accessor would be worse than either.
    let es = sim_complaints(
        "swarm(count = 8)",
        "    foreach p in flock {\n      p.vel = 1\n    }",
    );
    assert!(
        es.iter()
            .any(|e| e == "`flock` is not something this sim can iterate"),
        "{es:?}"
    );
}

#[test]
fn a_field_read_but_never_written_is_state_the_simulation_was_given() {
    // An element's fields are what the block *mentions*, read or written. A
    // body that integrates position from velocity without ever assigning
    // velocity is a complete and ordinary simulation: the velocities were set
    // when the elements were created and persist in the broadcast array.
    //
    // The cost is real and worth stating: a misspelled field is no longer an
    // error, it is a field that reads as whatever the array holds. Requiring an
    // assignment would catch the typo and forbid the simulation, which is the
    // worse trade - one is a wrong colour, the other is a thing that cannot be
    // written at all.
    let es = sim_complaints(
        "swarm(count = 8)",
        "    foreach p in swarm {
      p.pos = p.pos + p.vel
    }",
    );
    assert!(es.is_empty(), "{es:?}");
}

#[test]
fn a_field_assigned_later_is_readable_earlier() {
    // Which is what a simulation updating velocity from position and then
    // position from velocity does, so the fields are collected before anything
    // is checked rather than as each statement is reached.
    let es = sim_complaints(
        "swarm(count = 8)",
        "    foreach p in swarm {\n      p.vel = p.pos\n      p.pos = p.vel\n    }",
    );
    assert!(es.is_empty(), "{es:?}");
}

#[test]
fn a_field_can_only_be_assigned_through_an_element() {
    let es = sim_complaints("swarm(count = 8)", "    q.vel = 1");
    assert!(
        es.iter().any(|e| e == "`q` is not an element of a sim"),
        "{es:?}"
    );
}

#[test]
fn a_bare_assignment_needs_a_binding_to_target() {
    let es = sim_complaints("swarm(count = 8)", "    a = 1");
    assert!(es.iter().any(|e| e == "unknown name `a`"), "{es:?}");
}

#[test]
fn a_sim_local_binding_is_assignable_and_goes_out_of_scope() {
    let es = sim_complaints("swarm(count = 8)", "    let a = 1\n    a = 2");
    assert!(es.is_empty(), "{es:?}");

    // And is not visible to the layers, which run on every device rather than
    // only on the sim master.
    let leaked = errors(&wrap(
        "  sim swarm(count = 8) {\n    let a = 1\n  }\n  layer l { color = rgb(a, 0, 0) }",
    ));
    assert!(
        leaked.iter().any(|e| e.contains("unknown name `a`")),
        "a sim-local binding escaped into a layer: {leaked:?}"
    );
}

#[test]
fn branches_inside_a_sim_are_checked() {
    // `if` exists only inside `sim`, because the pixel profile has no
    // data-dependent control flow - so this is the only place it can be wrong.
    let es = sim_complaints(
        "swarm(count = 8)",
        "    foreach p in swarm {
      if p.vel > 1 {
        p.vel = nowhere
      } else {
        p.vel = 0
      }
    }",
    );
    assert!(
        es.iter().any(|e| e.contains("unknown name `nowhere`")),
        "{es:?}"
    );
}

// ---- `dt` costs a register, and only for the effects that ask -------------

/// Registers an effect needs, and whether it declared that it reads `dt`.
fn shape(src: &str) -> (u8, bool) {
    let bytes = compiled(src).bytecode;
    let program = Program::parse(&bytes).expect("a program");
    (registers(src), program.reads_dt)
}

#[test]
fn an_effect_that_never_mentions_dt_keeps_the_register() {
    // The point of the header flag. `dt` needs a register held for it, and
    // charging every effect for one moved the shipped corpus from a worst case
    // of 30 registers to 31 and stopped the editor's own sample compiling.
    // Asserted as "is register 15 available", which is the property, rather
    // than by comparing two register counts - two different expressions need
    // different numbers of temporaries, so that comparison would measure the
    // expressions rather than the reservation.
    let plain = wrap("  layer l {\n    color = rgb(u, 0, 0)\n  }");
    let (_, reads) = shape(&plain);
    assert!(!reads);
    assert!(
        steps_of(&pixel(&plain))
            .iter()
            .any(|(_, a, _, _)| *a == R_DT),
        "an effect with no `dt` should be free to allocate register {R_DT}",
    );

    // And an effect that does read it never writes there, because that is where
    // the VM is going to put `dt` every frame.
    let uses = wrap("  layer l {\n    color = rgb(u * dt, 0, 0)\n  }");
    assert!(shape(&uses).1);
    assert!(
        steps_of(&pixel(&uses))
            .iter()
            .all(|(_, a, _, _)| *a != R_DT),
        "register {R_DT} holds `dt` here and must not be allocated over",
    );
}

#[test]
fn an_effect_that_reads_dt_declares_it_and_pays_for_it() {
    let (_, reads) = shape(&wrap("  layer l {\n    color = rgb(dt, 0, 0)\n  }"));
    assert!(
        reads,
        "the flag is what tells a device to supply `dt` at all"
    );
}

#[test]
fn dt_reached_through_a_binding_still_declares_it() {
    // The flag comes from what emission actually resolved, not from scanning
    // the source for a name - so it survives a `let`, an inlined function, or
    // anything else that puts distance between the mention and the use. A scan
    // would be a second implementation of "does this read `dt`" and would drift
    // silently, because being wrong in the safe direction only costs a register
    // nobody notices.
    let src = wrap("  let step = dt * 60\n  layer l {\n    color = rgb(step, 0, 0)\n  }");
    let (_, reads) = shape(&src);
    assert!(reads);
}
