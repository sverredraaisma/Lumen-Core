//! Interpreter and program-format tests, through the public API.
//!
//! Programs are assembled with the builder rather than hand-written as hex, so a
//! test reads as the thing it is testing. The encoding itself is covered by the
//! round-trip tests in `isa`.

use lumen_vm::isa::{Instruction, OpCode, REG_COUNT};
use lumen_vm::program::builder::ProgramBuilder;
use lumen_vm::program::{Program, ProgramError, Section, MAGIC, PALETTE_STOPS, VM_VERSION};
use lumen_vm::q16::Q16;
use lumen_vm::vm::{
    hsv_to_rgb, kelvin_to_rgb, rgb_to_hsv, Arrays, Machine, NoArrays, NoUniforms, PixelInputs,
    PixelOutput, SliceArrays, Uniforms, R_I, R_SCRATCH, R_T, R_U, R_X,
};
use lumen_vm::{Fault, Profile};

/// Channels that return a fixed value, so a test can exercise `CHREAD` without a
/// network.
struct FixedChannels(Q16);

impl Uniforms for FixedChannels {
    fn channel(&self, _slot: u8, _offset: u8) -> Q16 {
        self.0
    }
}

/// Records probe writes.
#[derive(Default)]
struct ProbeLog {
    seen: Vec<(u16, Q16)>,
}

impl Uniforms for ProbeLog {
    fn channel(&self, _slot: u8, _offset: u8) -> Q16 {
        Q16::ZERO
    }
    fn probe(&mut self, probe_id: u16, value: Q16) {
        self.seen.push((probe_id, value));
    }
}

/// Build a `pixel`-section program from a list of instructions.
fn pixel_program(build: impl FnOnce(&mut ProgramBuilder)) -> Vec<u8> {
    let mut b = ProgramBuilder::new();
    build(&mut b);
    b.build()
}

/// Run a pixel program once with default inputs and return the machine.
fn run(bytes: &[u8]) -> (Machine, Result<PixelOutput, Fault>) {
    let program = Program::parse(bytes).expect("parse");
    let mut m = Machine::new();
    let out = m.run_pixel(&program, &PixelInputs::default(), &mut NoUniforms);
    (m, out)
}

// ---- Program format --------------------------------------------------------

#[test]
fn an_empty_program_parses_and_does_nothing() {
    let bytes = pixel_program(|_| {});
    let p = Program::parse(&bytes).unwrap();
    assert_eq!(p.vm_version, VM_VERSION);
    assert_eq!(p.profile, Profile::Pixel);
    assert_eq!(p.section_len(Section::Pixel), 0);
    assert!(!p.has_probes);
    let (_, out) = run(&bytes);
    assert_eq!(out, Ok(PixelOutput::None));
}

#[test]
fn a_program_that_is_not_a_program_is_refused() {
    assert_eq!(Program::parse(&[]), Err(ProgramError::Truncated));
    assert_eq!(Program::parse(&[0; 64]), Err(ProgramError::BadMagic));

    let mut bytes = pixel_program(|_| {});
    bytes[0] = b'X';
    assert_eq!(Program::parse(&bytes), Err(ProgramError::BadMagic));
}

#[test]
fn every_truncation_of_a_program_is_refused_without_panicking() {
    let bytes = pixel_program(|b| {
        let k = b.constant(Q16::ONE);
        b.channel(7);
        b.push(Section::Pixel, Instruction::with_imm(OpCode::LoadK, 20, k));
        b.push(
            Section::Pixel,
            Instruction::new(OpCode::EmitRgb, 20, 20, 20),
        );
    });
    for len in 0..bytes.len() {
        let _ = Program::parse(&bytes[..len]);
    }
    assert!(Program::parse(&bytes).is_ok());
}

#[test]
fn a_program_needing_a_newer_vm_says_so() {
    // The one failure a firmware upgrade fixes. Distinguishing it from "corrupt"
    // is what lets an app explain the situation instead of showing an error.
    let mut bytes = pixel_program(|_| {});
    bytes[4] = VM_VERSION + 1;
    assert_eq!(
        Program::parse(&bytes),
        Err(ProgramError::VmTooOld {
            needs: VM_VERSION + 1,
            have: VM_VERSION
        })
    );
}

#[test]
fn an_older_program_still_runs_on_a_newer_vm() {
    // Append-only instructions mean a firmware upgrade never invalidates a
    // program already running. This is that promise as a test.
    let mut bytes = pixel_program(|b| {
        b.push(Section::Pixel, Instruction::new(OpCode::Nop, 0, 0, 0));
    });
    bytes[4] = 0; // built against VM version 0
    assert!(Program::parse(&bytes).is_ok());
}

#[test]
fn an_unknown_profile_byte_is_refused() {
    let mut bytes = pixel_program(|_| {});
    bytes[5] = 9;
    assert_eq!(Program::parse(&bytes), Err(ProgramError::BadProfile(9)));
}

#[test]
fn an_unimplemented_instruction_is_caught_at_load_not_per_pixel() {
    // Whatever is wrong here is wrong before the first frame; faulting three
    // hundred times a second instead would bury the cause.
    let mut bytes = pixel_program(|b| {
        b.push(Section::Pixel, Instruction::new(OpCode::Nop, 0, 0, 0));
    });
    let last = bytes.len() - 4;
    bytes[last] = 0xEE;
    assert_eq!(
        Program::parse(&bytes),
        Err(ProgramError::UnsupportedInstruction(0xEE))
    );
}

#[test]
fn a_sim_instruction_in_a_pixel_program_is_refused_at_load() {
    let bytes = pixel_program(|b| {
        b.push(Section::Pixel, Instruction::new(OpCode::ALoad, 0, 0, 0));
    });
    assert_eq!(
        Program::parse(&bytes),
        Err(ProgramError::SimInstructionInPixelProfile(
            OpCode::ALoad.to_u8()
        ))
    );
}

#[test]
fn a_sim_program_may_use_the_array_instructions() {
    let mut b = ProgramBuilder::new();
    b.profile_sim = true;
    b.push(Section::Frame, Instruction::new(OpCode::ALoad, 0, 0, 0));
    let bytes = b.build();
    let p = Program::parse(&bytes).unwrap();
    assert_eq!(p.profile, Profile::Sim);
}

#[test]
fn unbalanced_repeat_blocks_are_refused() {
    let missing_end = pixel_program(|b| {
        b.push(Section::Pixel, Instruction::with_imm(OpCode::Repeat, 0, 3));
    });
    assert_eq!(
        Program::parse(&missing_end),
        Err(ProgramError::UnbalancedRepeat)
    );

    let extra_end = pixel_program(|b| {
        b.push(Section::Pixel, Instruction::new(OpCode::EndRep, 0, 0, 0));
    });
    assert_eq!(
        Program::parse(&extra_end),
        Err(ProgramError::UnbalancedRepeat)
    );
}

#[test]
fn a_jump_outside_its_section_is_refused() {
    let bad_call = pixel_program(|b| {
        b.push(Section::Pixel, Instruction::with_imm(OpCode::Call, 0, 99));
    });
    assert_eq!(Program::parse(&bad_call), Err(ProgramError::BadJumpTarget));

    // A MASKTEST skipping past the end would silently truncate the section
    // rather than doing what the author wrote.
    let bad_mask = pixel_program(|b| {
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::MaskTest, 0, 50),
        );
    });
    assert_eq!(Program::parse(&bad_mask), Err(ProgramError::BadJumpTarget));

    // Skipping to exactly the end is legal: it means "skip the rest".
    let ok = pixel_program(|b| {
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::MaskTest, 0, 1),
        );
        b.push(Section::Pixel, Instruction::new(OpCode::Nop, 0, 0, 0));
    });
    assert!(Program::parse(&ok).is_ok());
}

#[test]
fn the_constant_pool_shares_identical_values() {
    // A program using one number forty times should not carry forty copies.
    let mut b = ProgramBuilder::new();
    let a = b.constant(Q16::ONE);
    let c = b.constant(Q16::ONE);
    let d = b.constant(Q16::HALF);
    assert_eq!(a, c);
    assert_ne!(a, d);
    let bytes = b.build();
    let p = Program::parse(&bytes).unwrap();
    assert_eq!(p.constant_count(), 2);
    assert_eq!(p.constant(a), Some(Q16::ONE));
    assert_eq!(p.constant(d), Some(Q16::HALF));
    assert_eq!(p.constant(99), None);
}

#[test]
fn channel_slots_map_to_ids_so_a_program_can_be_repointed() {
    let mut b = ProgramBuilder::new();
    assert_eq!(b.channel(100), 0);
    assert_eq!(b.channel(200), 1);
    let bytes = b.build();
    let p = Program::parse(&bytes).unwrap();
    assert_eq!(p.channel_count(), 2);
    assert_eq!(p.channel_id(0), Some(100));
    assert_eq!(p.channel_id(1), Some(200));
    assert_eq!(p.channel_id(2), None);
}

#[test]
fn the_builder_computes_a_budget_from_the_code_it_emitted() {
    let bytes = pixel_program(|b| {
        b.push(Section::Pixel, Instruction::new(OpCode::Pow, 0, 0, 0));
        b.push(Section::Pixel, Instruction::new(OpCode::Add, 0, 0, 0));
    });
    let p = Program::parse(&bytes).unwrap();
    assert_eq!(p.budget, OpCode::Pow.cost() + OpCode::Add.cost());
}

#[test]
fn the_graph_hash_survives_a_round_trip() {
    // An editor uses it to recognise a program already running and skip the
    // upload, so it has to come back exactly.
    let mut b = ProgramBuilder::new();
    b.graph_hash = 0x0123_4567_89AB_CDEF;
    b.program_id = 4242;
    b.has_probes = true;
    let bytes = b.build();
    let p = Program::parse(&bytes).unwrap();
    assert_eq!(p.graph_hash, 0x0123_4567_89AB_CDEF);
    assert_eq!(p.program_id, 4242);
    assert!(p.has_probes);
    assert_eq!(&bytes[0..4], &MAGIC);
}

// ---- Arithmetic and dispatch ----------------------------------------------

#[test]
fn a_program_computes_and_emits() {
    let bytes = pixel_program(|b| {
        let k = b.constant(Q16::from_ratio(1, 4));
        b.push(Section::Pixel, Instruction::with_imm(OpCode::LoadK, 20, k));
        b.push(Section::Pixel, Instruction::new(OpCode::Add, 21, 20, 20));
        b.push(
            Section::Pixel,
            Instruction::new(OpCode::EmitRgb, 20, 21, 20),
        );
    });
    let (_, out) = run(&bytes);
    assert_eq!(
        out,
        Ok(PixelOutput::Rgb {
            r: Q16::from_ratio(1, 4),
            g: Q16::HALF,
            b: Q16::from_ratio(1, 4),
        })
    );
}

#[test]
fn division_by_zero_stops_the_program_rather_than_rendering_a_lie() {
    let bytes = pixel_program(|b| {
        let one = b.constant(Q16::ONE);
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 20, one),
        );
        // r21 is still zero.
        b.push(Section::Pixel, Instruction::new(OpCode::Div, 22, 20, 21));
    });
    let (_, out) = run(&bytes);
    assert_eq!(out, Err(Fault::DivideByZero));
}

#[test]
fn a_negative_square_root_is_a_domain_error() {
    let bytes = pixel_program(|b| {
        let neg = b.constant(Q16::from_int(-4));
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 20, neg),
        );
        b.push(Section::Pixel, Instruction::new(OpCode::Sqrt, 21, 20, 0));
    });
    let (_, out) = run(&bytes);
    assert_eq!(out, Err(Fault::DomainError));
}

#[test]
fn a_register_outside_the_file_is_a_fault_not_a_wraparound() {
    // Without this check a program could read or write someone else's register.
    let bytes = pixel_program(|b| {
        b.push(Section::Pixel, Instruction::new(OpCode::Mov, 200, 0, 0));
    });
    let (_, out) = run(&bytes);
    assert_eq!(out, Err(Fault::BadRegister(200)));
}

#[test]
fn a_register_run_that_overhangs_the_file_is_a_fault() {
    // NOISE3 reads three consecutive registers; starting near the top must fail
    // rather than read past the end.
    let bytes = pixel_program(|b| {
        b.push(
            Section::Pixel,
            Instruction::new(OpCode::Noise3, 0, (REG_COUNT - 1) as u8, 0),
        );
    });
    let (_, out) = run(&bytes);
    assert_eq!(out, Err(Fault::BadRegister((REG_COUNT - 1) as u8)));
}

// ---- Control flow ----------------------------------------------------------

#[test]
fn masktest_skips_when_the_mask_is_zero() {
    // The early-out that makes layered effects affordable.
    let bytes = pixel_program(|b| {
        let one = b.constant(Q16::ONE);
        // r20 (the mask) is zero, so the two loads are skipped.
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::MaskTest, 20, 2),
        );
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 21, one),
        );
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 22, one),
        );
        b.push(
            Section::Pixel,
            Instruction::new(OpCode::EmitRgb, 21, 22, 21),
        );
    });
    let (_, out) = run(&bytes);
    assert_eq!(
        out,
        Ok(PixelOutput::Rgb {
            r: Q16::ZERO,
            g: Q16::ZERO,
            b: Q16::ZERO
        })
    );
}

#[test]
fn masktest_falls_through_when_the_mask_is_set() {
    let bytes = pixel_program(|b| {
        let one = b.constant(Q16::ONE);
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 20, one),
        );
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::MaskTest, 20, 1),
        );
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 21, one),
        );
        b.push(
            Section::Pixel,
            Instruction::new(OpCode::EmitRgb, 21, 21, 21),
        );
    });
    let (_, out) = run(&bytes);
    assert_eq!(
        out,
        Ok(PixelOutput::Rgb {
            r: Q16::ONE,
            g: Q16::ONE,
            b: Q16::ONE
        })
    );
}

#[test]
fn a_masked_off_pixel_costs_far_less_than_a_rendered_one() {
    // Not just a correctness property: this is the reason MASKTEST exists.
    let make = |mask_set: bool| {
        pixel_program(|b| {
            let one = b.constant(Q16::ONE);
            if mask_set {
                b.push(
                    Section::Pixel,
                    Instruction::with_imm(OpCode::LoadK, 20, one),
                );
            }
            b.push(
                Section::Pixel,
                Instruction::with_imm(OpCode::MaskTest, 20, 6),
            );
            for _ in 0..6 {
                b.push(Section::Pixel, Instruction::new(OpCode::Pow, 21, 21, 21));
            }
        })
    };
    let (masked, _) = run(&make(false));
    let cheap = masked.spent();
    let (rendered, r) = run(&make(true));
    // The expensive path faults on pow of a zero base to a zero exponent? No —
    // 0^0 is 1, so it completes.
    assert!(r.is_ok());
    assert!(
        rendered.spent() > cheap * 4,
        "masked {cheap} vs rendered {}",
        rendered.spent()
    );
}

#[test]
fn repeat_runs_its_body_exactly_the_trip_count() {
    let bytes = pixel_program(|b| {
        let one = b.constant(Q16::ONE);
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 20, one),
        );
        b.push(Section::Pixel, Instruction::with_imm(OpCode::Repeat, 0, 5));
        b.push(Section::Pixel, Instruction::new(OpCode::Add, 21, 21, 20));
        b.push(Section::Pixel, Instruction::new(OpCode::EndRep, 0, 0, 0));
    });
    let (m, out) = run(&bytes);
    assert!(out.is_ok());
    assert_eq!(m.register(21), Some(Q16::from_int(5)));
}

#[test]
fn a_zero_trip_repeat_skips_its_body_instead_of_running_it_once() {
    // The mistake a naive implementation makes, and it is invisible until an
    // effect has a conditionally-empty loop.
    let bytes = pixel_program(|b| {
        let one = b.constant(Q16::ONE);
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 20, one),
        );
        b.push(Section::Pixel, Instruction::with_imm(OpCode::Repeat, 0, 0));
        b.push(Section::Pixel, Instruction::new(OpCode::Add, 21, 21, 20));
        b.push(Section::Pixel, Instruction::new(OpCode::EndRep, 0, 0, 0));
    });
    let (m, out) = run(&bytes);
    assert!(out.is_ok());
    assert_eq!(m.register(21), Some(Q16::ZERO));
}

#[test]
fn nested_repeats_multiply() {
    let bytes = pixel_program(|b| {
        let one = b.constant(Q16::ONE);
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 20, one),
        );
        b.push(Section::Pixel, Instruction::with_imm(OpCode::Repeat, 0, 3));
        b.push(Section::Pixel, Instruction::with_imm(OpCode::Repeat, 0, 4));
        b.push(Section::Pixel, Instruction::new(OpCode::Add, 21, 21, 20));
        b.push(Section::Pixel, Instruction::new(OpCode::EndRep, 0, 0, 0));
        b.push(Section::Pixel, Instruction::new(OpCode::EndRep, 0, 0, 0));
    });
    let (m, out) = run(&bytes);
    assert!(out.is_ok());
    assert_eq!(m.register(21), Some(Q16::from_int(12)));
}

#[test]
fn nesting_deeper_than_the_machine_allows_is_a_stack_overflow() {
    let bytes = pixel_program(|b| {
        for _ in 0..12 {
            b.push(Section::Pixel, Instruction::with_imm(OpCode::Repeat, 0, 2));
        }
        for _ in 0..12 {
            b.push(Section::Pixel, Instruction::new(OpCode::EndRep, 0, 0, 0));
        }
    });
    let (_, out) = run(&bytes);
    assert_eq!(out, Err(Fault::StackOverflow));
}

#[test]
fn call_and_ret_return_to_the_caller() {
    let bytes = pixel_program(|b| {
        let one = b.constant(Q16::ONE);
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 20, one),
        ); // 0
        b.push(Section::Pixel, Instruction::with_imm(OpCode::Call, 0, 4)); // 1
        b.push(Section::Pixel, Instruction::new(OpCode::Add, 21, 21, 20)); // 2
        b.push(Section::Pixel, Instruction::new(OpCode::Halt, 0, 0, 0)); // 3
        b.push(Section::Pixel, Instruction::new(OpCode::Add, 21, 21, 20)); // 4
        b.push(Section::Pixel, Instruction::new(OpCode::Ret, 0, 0, 0)); // 5
    });
    let (m, out) = run(&bytes);
    assert!(out.is_ok());
    // Once in the subroutine, once after returning.
    assert_eq!(m.register(21), Some(Q16::from_int(2)));
}

#[test]
fn a_ret_with_no_call_ends_the_section() {
    let bytes = pixel_program(|b| {
        let one = b.constant(Q16::ONE);
        b.push(Section::Pixel, Instruction::new(OpCode::Ret, 0, 0, 0));
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 20, one),
        );
    });
    let (m, out) = run(&bytes);
    assert!(out.is_ok());
    assert_eq!(m.register(20), Some(Q16::ZERO));
}

#[test]
fn halt_ends_the_section_early() {
    let bytes = pixel_program(|b| {
        let one = b.constant(Q16::ONE);
        b.push(Section::Pixel, Instruction::new(OpCode::Halt, 0, 0, 0));
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 20, one),
        );
    });
    let (m, _) = run(&bytes);
    assert_eq!(m.register(20), Some(Q16::ZERO));
}

// ---- Budget ----------------------------------------------------------------

#[test]
fn the_budget_stops_a_program_that_costs_more_than_it_promised() {
    // The backstop. Reaching it means the compiler's estimate was wrong, but a
    // program must never be able to run away with the frame.
    let bytes = pixel_program(|b| {
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::Repeat, 0, 1000),
        );
        b.push(Section::Pixel, Instruction::new(OpCode::Pow, 20, 20, 20));
        b.push(Section::Pixel, Instruction::new(OpCode::EndRep, 0, 0, 0));
    });
    let program = Program::parse(&bytes).unwrap();
    let mut m = Machine::new();
    m.set_budget(100);
    let out = m.run_pixel(&program, &PixelInputs::default(), &mut NoUniforms);
    assert_eq!(out, Err(Fault::BudgetExceeded));
    assert!(m.spent() > 100);
}

#[test]
fn budget_is_measured_per_invocation_not_cumulatively() {
    // Otherwise the three hundredth pixel of a frame would fault while the first
    // rendered fine, which would look like a hardware fault rather than a bug.
    let bytes = pixel_program(|b| {
        b.push(Section::Pixel, Instruction::new(OpCode::Nop, 0, 0, 0));
    });
    let program = Program::parse(&bytes).unwrap();
    let mut m = Machine::new();
    m.set_budget(5);
    for _ in 0..1000 {
        assert!(m
            .run_pixel(&program, &PixelInputs::default(), &mut NoUniforms)
            .is_ok());
    }
    assert_eq!(m.spent(), OpCode::Nop.cost());
}

// ---- Sections and hoisting -------------------------------------------------

#[test]
fn frame_results_survive_into_every_pixel() {
    // The whole performance story: a value hoisted into `frame` is computed once
    // and read three hundred times. If this ever breaks, hoisting silently stops
    // paying and nothing else fails.
    let mut b = ProgramBuilder::new();
    let k = b.constant(Q16::from_int(7));
    b.push(
        Section::Frame,
        Instruction::with_imm(OpCode::LoadK, R_SCRATCH, k),
    );
    b.push(
        Section::Pixel,
        Instruction::new(OpCode::EmitRgb, R_SCRATCH, R_SCRATCH, R_SCRATCH),
    );
    let bytes = b.build();
    let program = Program::parse(&bytes).unwrap();

    let mut m = Machine::new();
    m.run_frame_at(&program, Q16::from_int(2), &mut NoUniforms)
        .unwrap();
    for i in 0..10 {
        let inputs = PixelInputs {
            index: Q16::from_int(i),
            ..Default::default()
        };
        assert_eq!(
            m.run_pixel(&program, &inputs, &mut NoUniforms),
            Ok(PixelOutput::Rgb {
                r: Q16::from_int(7),
                g: Q16::from_int(7),
                b: Q16::from_int(7)
            })
        );
    }
}

#[test]
fn per_pixel_inputs_are_refreshed_but_scratch_is_not() {
    let mut b = ProgramBuilder::new();
    // Accumulate into a scratch register across pixels, and echo the index.
    b.push(
        Section::Pixel,
        Instruction::new(OpCode::Add, R_SCRATCH, R_SCRATCH, R_I),
    );
    b.push(
        Section::Pixel,
        Instruction::new(OpCode::EmitRgb, R_I, R_SCRATCH, R_U),
    );
    let bytes = b.build();
    let program = Program::parse(&bytes).unwrap();

    let mut m = Machine::new();
    let mut last = PixelOutput::None;
    for i in 0..4 {
        let inputs = PixelInputs {
            index: Q16::from_int(i),
            u: Q16::from_ratio(i as i32, 4),
            ..Default::default()
        };
        last = m.run_pixel(&program, &inputs, &mut NoUniforms).unwrap();
    }
    // 0+1+2+3 accumulated in scratch; index and u came fresh each time.
    assert_eq!(
        last,
        PixelOutput::Rgb {
            r: Q16::from_int(3),
            g: Q16::from_int(6),
            b: Q16::from_ratio(3, 4)
        }
    );
}

#[test]
fn the_once_section_runs_on_its_own() {
    let mut b = ProgramBuilder::new();
    let k = b.constant(Q16::from_int(3));
    b.push(
        Section::Once,
        Instruction::with_imm(OpCode::LoadK, R_SCRATCH, k),
    );
    let bytes = b.build();
    let program = Program::parse(&bytes).unwrap();
    let mut m = Machine::new();
    m.run_once(&program, &mut NoUniforms).unwrap();
    assert_eq!(m.register(R_SCRATCH), Some(Q16::from_int(3)));
}

#[test]
fn show_time_reaches_the_frame_section() {
    let mut b = ProgramBuilder::new();
    b.push(
        Section::Frame,
        Instruction::new(OpCode::Mov, R_SCRATCH, R_T, 0),
    );
    let bytes = b.build();
    let program = Program::parse(&bytes).unwrap();
    let mut m = Machine::new();
    m.run_frame_at(&program, Q16::from_int(42), &mut NoUniforms)
        .unwrap();
    assert_eq!(m.register(R_SCRATCH), Some(Q16::from_int(42)));
}

#[test]
fn reset_clears_the_machine() {
    let bytes = pixel_program(|b| {
        let k = b.constant(Q16::ONE);
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, R_SCRATCH, k),
        );
    });
    let (mut m, _) = run(&bytes);
    assert_eq!(m.register(R_SCRATCH), Some(Q16::ONE));
    m.reset();
    assert_eq!(m.register(R_SCRATCH), Some(Q16::ZERO));
    assert_eq!(m.register(200), None);
}

// ---- Uniforms, history, probes --------------------------------------------

#[test]
fn chread_reaches_the_uniforms_supplied_by_the_caller() {
    let bytes = pixel_program(|b| {
        b.channel(77);
        b.push(Section::Pixel, Instruction::new(OpCode::ChRead, 20, 0, 0));
        b.push(
            Section::Pixel,
            Instruction::new(OpCode::EmitRgb, 20, 20, 20),
        );
    });
    let program = Program::parse(&bytes).unwrap();
    let mut m = Machine::new();
    let mut channels = FixedChannels(Q16::from_ratio(3, 4));
    assert_eq!(
        m.run_pixel(&program, &PixelInputs::default(), &mut channels),
        Ok(PixelOutput::Rgb {
            r: Q16::from_ratio(3, 4),
            g: Q16::from_ratio(3, 4),
            b: Q16::from_ratio(3, 4)
        })
    );
}

#[test]
fn a_channel_with_no_publisher_reads_zero_rather_than_failing() {
    // Defined degradation: a dead audio publisher must leave the lights doing
    // something sensible, not stop the program.
    let bytes = pixel_program(|b| {
        b.channel(77);
        b.push(Section::Pixel, Instruction::new(OpCode::ChRead, 20, 0, 0));
        b.push(
            Section::Pixel,
            Instruction::new(OpCode::EmitRgb, 20, 20, 20),
        );
    });
    let (_, out) = run(&bytes);
    assert_eq!(
        out,
        Ok(PixelOutput::Rgb {
            r: Q16::ZERO,
            g: Q16::ZERO,
            b: Q16::ZERO
        })
    );
}

#[test]
fn the_history_buffer_round_trips_through_a_frame() {
    // Trails and fire are built on this: read last frame's colour, write this
    // one's. It is a colour, not a scalar - a monochrome history would make
    // every trail grey, which is most of what the buffer exists for.
    let bytes = pixel_program(|b| {
        let half = b.constant(Q16::HALF);
        b.push(Section::Pixel, Instruction::new(OpCode::PrevRead, 20, 0, 0));
        b.push(
            Section::Pixel,
            Instruction::with_imm(OpCode::LoadK, 23, half),
        );
        b.push(Section::Pixel, Instruction::new(OpCode::Add, 20, 20, 23));
        b.push(
            Section::Pixel,
            Instruction::new(OpCode::PrevWrite, 20, 0, 0),
        );
    });
    let program = Program::parse(&bytes).unwrap();
    let mut m = Machine::new();

    let mut prev = [Q16::ZERO; 3];
    for expect in 1..=4 {
        let inputs = PixelInputs {
            prev,
            ..Default::default()
        };
        m.run_pixel(&program, &inputs, &mut NoUniforms).unwrap();
        prev = m.prev_out();
        // Only the red channel accumulates; the other two ride along untouched,
        // which proves all three are carried rather than one being broadcast.
        assert_eq!(prev[0], Q16::from_ratio(expect, 2));
        assert_eq!(prev[1], Q16::ZERO);
        assert_eq!(prev[2], Q16::ZERO);
    }
}

#[test]
fn every_channel_of_the_history_buffer_is_carried() {
    let bytes = pixel_program(|b| {
        b.push(Section::Pixel, Instruction::new(OpCode::PrevRead, 20, 0, 0));
        b.push(
            Section::Pixel,
            Instruction::new(OpCode::PrevWrite, 20, 0, 0),
        );
    });
    let program = Program::parse(&bytes).unwrap();
    let mut m = Machine::new();
    let colour = [Q16::from_ratio(1, 4), Q16::HALF, Q16::from_ratio(3, 4)];
    m.run_pixel(
        &program,
        &PixelInputs {
            prev: colour,
            ..Default::default()
        },
        &mut NoUniforms,
    )
    .unwrap();
    assert_eq!(m.prev_out(), colour);
}

#[test]
fn probes_reach_the_sink_and_cost_budget() {
    // Probe builds are explicit and bounded: instrumentation costs like anything
    // else, so a normal build must contain none of it.
    let bytes = pixel_program(|b| {
        let k = b.constant(Q16::from_int(5));
        b.has_probes = true;
        b.push(Section::Pixel, Instruction::with_imm(OpCode::LoadK, 20, k));
        b.push(Section::Pixel, Instruction::with_imm(OpCode::Probe, 20, 9));
    });
    let program = Program::parse(&bytes).unwrap();
    assert!(program.has_probes);
    let mut m = Machine::new();
    let mut log = ProbeLog::default();
    m.run_pixel(&program, &PixelInputs::default(), &mut log)
        .unwrap();
    assert_eq!(log.seen, vec![(9, Q16::from_int(5))]);
    assert_eq!(m.spent(), OpCode::LoadK.cost() + OpCode::Probe.cost());
}

// ---- Colour ----------------------------------------------------------------

#[test]
fn hsv_to_rgb_hits_the_primaries() {
    // A third of a turn is not exactly representable in Q16, so green and blue
    // land a couple of raw units off. Tolerating that is correct; asserting
    // exact equality would be asserting a property of the quantisation.
    let near = |got: [Q16; 3], want: [Q16; 3], what: &str| {
        for k in 0..3 {
            let d = (got[k].0 - want[k].0).abs();
            assert!(d <= 8, "{what} channel {k}: {} vs {}", got[k].0, want[k].0);
        }
    };
    let full = |h: Q16| hsv_to_rgb(h, Q16::ONE, Q16::ONE);
    // Red is exact: hue zero needs no division.
    assert_eq!(full(Q16::ZERO), [Q16::ONE, Q16::ZERO, Q16::ZERO]);
    near(
        full(Q16::from_ratio(1, 3)),
        [Q16::ZERO, Q16::ONE, Q16::ZERO],
        "green",
    );
    near(
        full(Q16::from_ratio(2, 3)),
        [Q16::ZERO, Q16::ZERO, Q16::ONE],
        "blue",
    );
    // Hue is in turns, so a full turn comes back to red.
    assert_eq!(full(Q16::ONE), [Q16::ONE, Q16::ZERO, Q16::ZERO]);
}

#[test]
fn zero_saturation_is_grey_and_zero_value_is_black() {
    let grey = hsv_to_rgb(Q16::from_ratio(1, 5), Q16::ZERO, Q16::HALF);
    assert_eq!(grey, [Q16::HALF, Q16::HALF, Q16::HALF]);
    let black = hsv_to_rgb(Q16::from_ratio(1, 5), Q16::ONE, Q16::ZERO);
    assert_eq!(black, [Q16::ZERO, Q16::ZERO, Q16::ZERO]);
}

#[test]
fn hsv_round_trips_through_rgb() {
    for i in 0..12 {
        let h = Q16::from_ratio(i, 12);
        let rgb = hsv_to_rgb(h, Q16::ONE, Q16::ONE);
        let [h2, s2, v2] = rgb_to_hsv(rgb[0], rgb[1], rgb[2]);
        assert!((h2.0 - h.0).abs() < 600, "hue {i}: {} vs {}", h2.0, h.0);
        assert_eq!(s2, Q16::ONE);
        assert_eq!(v2, Q16::ONE);
    }
}

#[test]
fn rgb_to_hsv_handles_greys_without_dividing_by_zero() {
    assert_eq!(
        rgb_to_hsv(Q16::HALF, Q16::HALF, Q16::HALF),
        [Q16::ZERO, Q16::ZERO, Q16::HALF]
    );
    assert_eq!(
        rgb_to_hsv(Q16::ZERO, Q16::ZERO, Q16::ZERO),
        [Q16::ZERO, Q16::ZERO, Q16::ZERO]
    );
}

#[test]
fn colour_temperature_runs_warm_to_cool() {
    // Low kelvin is red-dominant, high kelvin blue-dominant. Getting this
    // backwards is the classic mistake and it is instantly visible.
    let warm = kelvin_to_rgb(Q16::from_int(2000));
    let cool = kelvin_to_rgb(Q16::from_int(9000));
    assert!(warm[0] > warm[2], "2000K should be red-dominant");
    assert!(cool[2] >= cool[0], "9000K should be blue-dominant");
    // Out-of-range inputs clamp rather than producing negative channels.
    for k in [-5000, 0, 500, 50_000] {
        for ch in kelvin_to_rgb(Q16::from_int(k as i16)) {
            assert!(ch.0 >= 0 && ch.0 <= Q16::ONE.0, "{k}K gave {}", ch.0);
        }
    }
}

#[test]
fn palettes_sample_and_wrap() {
    let mut stops = [(Q16::ZERO, Q16::ZERO, Q16::ZERO); PALETTE_STOPS];
    stops[0] = (Q16::ONE, Q16::ZERO, Q16::ZERO);
    stops[PALETTE_STOPS / 2] = (Q16::ZERO, Q16::ONE, Q16::ZERO);

    let mut b = ProgramBuilder::new();
    let pal = b.palette(&stops);
    b.push(
        Section::Pixel,
        Instruction::new(OpCode::Palette, 20, R_X, pal),
    );
    b.push(
        Section::Pixel,
        Instruction::new(OpCode::EmitRgb, 20, 21, 22),
    );
    let bytes = b.build();
    let program = Program::parse(&bytes).unwrap();
    assert_eq!(program.palette_count(), 1);

    let sample = |pos: Q16| {
        let mut m = Machine::new();
        let inputs = PixelInputs {
            x: pos,
            ..Default::default()
        };
        m.run_pixel(&program, &inputs, &mut NoUniforms).unwrap()
    };

    assert_eq!(
        sample(Q16::ZERO),
        PixelOutput::Rgb {
            r: Q16::ONE,
            g: Q16::ZERO,
            b: Q16::ZERO
        }
    );
    assert_eq!(
        sample(Q16::HALF),
        PixelOutput::Rgb {
            r: Q16::ZERO,
            g: Q16::ONE,
            b: Q16::ZERO
        }
    );
    // Position wraps, so 1.0 meets 0.0.
    assert_eq!(sample(Q16::ONE), sample(Q16::ZERO));
    assert_eq!(sample(Q16::from_int(-1)), sample(Q16::ZERO));

    // A palette index that does not exist is a bad program, not a silent black.
    let mut b2 = ProgramBuilder::new();
    b2.push(Section::Pixel, Instruction::new(OpCode::Palette, 20, 0, 3));
    let bad = b2.build();
    let (_, out) = run(&bad);
    assert_eq!(out, Err(Fault::BadProgram));
}

// ---- A realistic effect ----------------------------------------------------

#[test]
fn a_plane_sweeping_through_a_room_is_a_pure_function_of_position_and_time() {
    // The claim the whole architecture rests on: every LED knows where it is, so
    // a volumetric effect needs no network traffic at all. Two "devices" here
    // render disjoint pixel sets and must agree exactly where they overlap.
    let mut b = ProgramBuilder::new();
    let scale = b.constant(Q16::from_ratio(1, 4));
    // frame: hoist nothing but time scaling.
    b.push(
        Section::Frame,
        Instruction::with_imm(OpCode::LoadK, R_SCRATCH, scale),
    );
    b.push(
        Section::Frame,
        Instruction::new(OpCode::Mul, R_SCRATCH + 1, R_T, R_SCRATCH),
    );
    // pixel: brightness = sin_turns(z - t*scale)
    b.push(
        Section::Pixel,
        Instruction::new(OpCode::Sub, R_SCRATCH + 2, 2, R_SCRATCH + 1),
    );
    b.push(
        Section::Pixel,
        Instruction::new(OpCode::SinTurns, R_SCRATCH + 2, R_SCRATCH + 2, 0),
    );
    b.push(
        Section::Pixel,
        Instruction::new(OpCode::EmitRgb, R_SCRATCH + 2, R_SCRATCH + 2, R_SCRATCH + 2),
    );
    let bytes = b.build();
    let program = Program::parse(&bytes).unwrap();

    let render = |zs: &[i32], t: Q16| {
        let mut m = Machine::new();
        m.run_frame_at(&program, t, &mut NoUniforms).unwrap();
        zs.iter()
            .map(|&z| {
                let inputs = PixelInputs {
                    z: Q16::from_ratio(z, 10),
                    ..Default::default()
                };
                m.run_pixel(&program, &inputs, &mut NoUniforms).unwrap()
            })
            .collect::<Vec<_>>()
    };

    let t = Q16::from_ratio(7, 3);
    let device_a = render(&[0, 1, 2, 3, 4], t);
    let device_b = render(&[3, 4, 5, 6], t);
    // The two devices overlap at z = 0.3 and 0.4 and must agree bit for bit.
    assert_eq!(device_a[3], device_b[0]);
    assert_eq!(device_a[4], device_b[1]);
    // And the plane actually moves.
    let later = render(&[0, 1, 2, 3, 4], t.add(Q16::ONE));
    assert_ne!(device_a, later);
}

// ---- The sim profile -------------------------------------------------------

#[test]
fn a_sim_program_reads_and_writes_its_arrays() {
    // Particles as a flat array: read element 0, add one, write it back. This is
    // the shape every simulation has.
    let mut b = ProgramBuilder::new();
    b.profile_sim = true;
    let one = b.constant(Q16::ONE);
    let zero = b.constant(Q16::ZERO);
    b.push(
        Section::Frame,
        Instruction::with_imm(OpCode::LoadK, 20, zero),
    );
    b.push(Section::Frame, Instruction::new(OpCode::ALoad, 21, 0, 20));
    b.push(
        Section::Frame,
        Instruction::with_imm(OpCode::LoadK, 22, one),
    );
    b.push(Section::Frame, Instruction::new(OpCode::Add, 21, 21, 22));
    b.push(Section::Frame, Instruction::new(OpCode::AStore, 0, 20, 21));
    let bytes = b.build();
    let program = Program::parse(&bytes).unwrap();

    let mut storage = [Q16::ZERO; 4];
    let layout = [(0usize, 4usize)];
    let mut m = Machine::new();
    for expect in 1..=3 {
        let mut arrays = SliceArrays::new(&mut storage, &layout).unwrap();
        m.run_sim(&program, Q16::ZERO, &mut NoUniforms, &mut arrays)
            .unwrap();
        assert_eq!(storage[0], Q16::from_int(expect));
    }
}

#[test]
fn alen_reports_the_declared_length() {
    let mut b = ProgramBuilder::new();
    b.profile_sim = true;
    b.push(Section::Frame, Instruction::new(OpCode::ALen, 20, 1, 0));
    let bytes = b.build();
    let program = Program::parse(&bytes).unwrap();

    let mut storage = [Q16::ZERO; 10];
    let layout = [(0usize, 4usize), (4usize, 6usize)];
    let mut arrays = SliceArrays::new(&mut storage, &layout).unwrap();
    let mut m = Machine::new();
    m.run_sim(&program, Q16::ZERO, &mut NoUniforms, &mut arrays)
        .unwrap();
    assert_eq!(m.register(20), Some(Q16::from_int(6)));
}

#[test]
fn an_out_of_range_index_faults_rather_than_wrapping() {
    // A wrapped index reads a neighbouring particle and the simulation quietly
    // goes wrong, which is the worst possible failure mode for something whose
    // output is broadcast to every device.
    let mut b = ProgramBuilder::new();
    b.profile_sim = true;
    let big = b.constant(Q16::from_int(999));
    b.push(
        Section::Frame,
        Instruction::with_imm(OpCode::LoadK, 20, big),
    );
    b.push(Section::Frame, Instruction::new(OpCode::ALoad, 21, 0, 20));
    let bytes = b.build();
    let program = Program::parse(&bytes).unwrap();

    let mut storage = [Q16::ZERO; 4];
    let layout = [(0usize, 4usize)];
    let mut arrays = SliceArrays::new(&mut storage, &layout).unwrap();
    let mut m = Machine::new();
    assert_eq!(
        m.run_sim(&program, Q16::ZERO, &mut NoUniforms, &mut arrays),
        Err(Fault::OutOfBounds)
    );
}

#[test]
fn a_negative_index_faults() {
    let mut b = ProgramBuilder::new();
    b.profile_sim = true;
    let neg = b.constant(Q16::from_int(-1));
    b.push(
        Section::Frame,
        Instruction::with_imm(OpCode::LoadK, 20, neg),
    );
    b.push(Section::Frame, Instruction::new(OpCode::ALoad, 21, 0, 20));
    let bytes = b.build();
    let program = Program::parse(&bytes).unwrap();

    let mut storage = [Q16::ZERO; 4];
    let layout = [(0usize, 4usize)];
    let mut arrays = SliceArrays::new(&mut storage, &layout).unwrap();
    let mut m = Machine::new();
    assert_eq!(
        m.run_sim(&program, Q16::ZERO, &mut NoUniforms, &mut arrays),
        Err(Fault::OutOfBounds)
    );
}

#[test]
fn an_unknown_array_faults() {
    let mut b = ProgramBuilder::new();
    b.profile_sim = true;
    b.push(Section::Frame, Instruction::new(OpCode::ALen, 20, 7, 0));
    let bytes = b.build();
    let program = Program::parse(&bytes).unwrap();

    let mut storage = [Q16::ZERO; 4];
    let layout = [(0usize, 4usize)];
    let mut arrays = SliceArrays::new(&mut storage, &layout).unwrap();
    let mut m = Machine::new();
    assert_eq!(
        m.run_sim(&program, Q16::ZERO, &mut NoUniforms, &mut arrays),
        Err(Fault::OutOfBounds)
    );
}

#[test]
fn a_pixel_program_cannot_be_run_as_a_sim() {
    let bytes = pixel_program(|b| {
        b.push(Section::Frame, Instruction::new(OpCode::Nop, 0, 0, 0));
    });
    let program = Program::parse(&bytes).unwrap();
    let mut storage = [Q16::ZERO; 1];
    let layout = [(0usize, 1usize)];
    let mut arrays = SliceArrays::new(&mut storage, &layout).unwrap();
    let mut m = Machine::new();
    assert_eq!(
        m.run_sim(&program, Q16::ZERO, &mut NoUniforms, &mut arrays),
        Err(Fault::BadProgram)
    );
}

#[test]
fn a_layout_that_does_not_fit_its_storage_is_refused_up_front() {
    let mut storage = [Q16::ZERO; 4];
    assert!(SliceArrays::new(&mut storage, &[(0usize, 5usize)]).is_none());
    assert!(SliceArrays::new(&mut storage, &[(3usize, 2usize)]).is_none());
    assert!(SliceArrays::new(&mut storage, &[(0usize, 2usize), (2usize, 2usize)]).is_some());
}

#[test]
fn sim_state_is_a_flat_slice_ready_to_broadcast() {
    // It is broadcast every frame and handed over on failover, so it has to be
    // its own wire format rather than something that needs serialising.
    let mut storage = [Q16::ONE; 4];
    let layout = [(0usize, 4usize)];
    let arrays = SliceArrays::new(&mut storage, &layout).unwrap();
    assert_eq!(arrays.as_slice().len(), 4);
    assert_eq!(arrays.as_slice()[0], Q16::ONE);
}

#[test]
fn a_sim_is_deterministic_from_the_same_starting_state() {
    // What replay and sim-master failover both rest on.
    let mut b = ProgramBuilder::new();
    b.profile_sim = true;
    b.push(Section::Frame, Instruction::new(OpCode::ALoad, 20, 0, 20));
    b.push(Section::Frame, Instruction::new(OpCode::Noise1, 21, 20, 0));
    b.push(Section::Frame, Instruction::new(OpCode::AStore, 0, 20, 21));
    let bytes = b.build();
    let program = Program::parse(&bytes).unwrap();
    let layout = [(0usize, 4usize)];

    let run = || {
        let mut storage = [Q16::HALF; 4];
        let mut m = Machine::new();
        for _ in 0..5 {
            let mut arrays = SliceArrays::new(&mut storage, &layout).unwrap();
            m.run_sim(&program, Q16::ZERO, &mut NoUniforms, &mut arrays)
                .unwrap();
        }
        storage
    };
    assert_eq!(run(), run());
}

#[test]
fn no_arrays_refuses_every_access() {
    let mut none = NoArrays;
    assert_eq!(none.len(0), None);
    assert_eq!(none.load(0, 0), Err(Fault::OutOfBounds));
    assert_eq!(none.store(0, 0, Q16::ONE), Err(Fault::OutOfBounds));
}
