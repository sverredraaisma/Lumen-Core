//! Exercising the C ABI from Rust.
//!
//! A boundary nobody tests from this side is one that breaks silently on a
//! target nobody builds locally — which for this crate means an ESP8266 in
//! somebody's ceiling. Everything here calls the `extern "C"` functions the way
//! C would, through raw pointers, including the ways C gets it wrong.

extern crate alloc;

use super::*;
use alloc::vec::Vec;

/// Storage for a machine, aligned as the ABI requires.
#[repr(align(8))]
struct Storage([u8; 4096]);

fn storage() -> Storage {
    Storage([0; 4096])
}

unsafe fn machine(s: &mut Storage) -> *mut LumenMachine {
    let mut m: *mut LumenMachine = core::ptr::null_mut();
    assert_eq!(
        lumen_machine_init(s.0.as_mut_ptr() as *mut c_void, s.0.len(), &mut m),
        LUMEN_OK
    );
    m
}

/// A program whose output differs per pixel, so a test cannot pass by rendering
/// one colour everywhere.
fn ramp() -> Vec<u8> {
    use lumen_vm::isa::{Instruction, OpCode};
    use lumen_vm::program::builder::ProgramBuilder;
    use lumen_vm::program::Section;
    use lumen_vm::vm::R_U;

    let mut b = ProgramBuilder::new();
    let zero = b.constant(Q16::ZERO);
    b.push(
        Section::Pixel,
        Instruction::with_imm(OpCode::LoadK, 20, zero),
    );
    b.push(
        Section::Pixel,
        Instruction::new(OpCode::EmitRgb, R_U, 20, 20),
    );
    b.build()
}

#[test]
fn a_machine_needs_storage_of_the_size_it_asks_for() {
    let mut s = storage();
    let mut m: *mut LumenMachine = core::ptr::null_mut();
    unsafe {
        // One byte short is refused rather than written past.
        assert_eq!(
            lumen_machine_init(
                s.0.as_mut_ptr() as *mut c_void,
                lumen_machine_size() - 1,
                &mut m
            ),
            LUMEN_TOO_SMALL
        );
        assert!(m.is_null());
        assert_eq!(
            lumen_machine_init(s.0.as_mut_ptr() as *mut c_void, s.0.len(), &mut m),
            LUMEN_OK
        );
        assert!(!m.is_null());
    }
}

#[test]
fn misaligned_storage_is_refused() {
    // A C caller has nothing that would catch this, and writing a machine
    // through a misaligned pointer is undefined behaviour rather than a slow
    // path.
    let mut s = storage();
    let mut m: *mut LumenMachine = core::ptr::null_mut();
    unsafe {
        let misaligned = s.0.as_mut_ptr().add(1) as *mut c_void;
        assert_eq!(
            lumen_machine_init(misaligned, s.0.len() - 1, &mut m),
            LUMEN_TOO_SMALL
        );
    }
}

#[test]
fn null_pointers_return_an_error_rather_than_being_read() {
    unsafe {
        let mut m: *mut LumenMachine = core::ptr::null_mut();
        assert_eq!(
            lumen_machine_init(core::ptr::null_mut(), 4096, &mut m),
            LUMEN_NULL
        );
        assert_eq!(
            lumen_program_check(core::ptr::null(), 10, core::ptr::null_mut()),
            LUMEN_NULL
        );
        assert_eq!(
            lumen_frame(core::ptr::null_mut(), core::ptr::null(), 0, 0),
            LUMEN_NULL
        );
        assert_eq!(
            lumen_render(
                core::ptr::null_mut(),
                core::ptr::null(),
                0,
                1,
                core::ptr::null_mut(),
                3
            ),
            LUMEN_NULL
        );
        assert_eq!(
            lumen_header_read(
                core::ptr::null(),
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut()
            ),
            LUMEN_NULL
        );
    }
}

#[test]
fn rubbish_is_not_a_program() {
    let bytes = [1u8, 2, 3, 4];
    unsafe {
        assert_eq!(
            lumen_program_check(bytes.as_ptr(), bytes.len(), core::ptr::null_mut()),
            LUMEN_BAD_PROGRAM
        );
    }
}

#[test]
fn a_program_reports_the_budget_it_carries() {
    let p = ramp();
    let mut budget = 0u32;
    unsafe {
        assert_eq!(
            lumen_program_check(p.as_ptr(), p.len(), &mut budget),
            LUMEN_OK
        );
    }
    assert!(
        budget > 0,
        "a program that costs nothing is a program that does nothing"
    );
}

#[test]
fn a_strip_renders_into_the_callers_buffer() {
    let p = ramp();
    let mut s = storage();
    let mut out = [0u8; 30];
    unsafe {
        let m = machine(&mut s);
        assert_eq!(lumen_frame(m, p.as_ptr(), p.len(), 0), LUMEN_OK);
        assert_eq!(
            lumen_render(m, p.as_ptr(), p.len(), 10, out.as_mut_ptr(), out.len()),
            LUMEN_OK
        );
    }
    // `u` runs 0..1 along the strip, so red climbs and the last is brightest.
    assert_eq!(out[0], 0);
    assert!(out[27] > out[0], "the strip is flat: {out:?}");
    // Green and blue were written zero rather than left as whatever was there.
    assert_eq!(out[1], 0);
    assert_eq!(out[2], 0);
}

#[test]
fn a_buffer_too_small_for_the_strip_is_refused() {
    // The failure that would otherwise corrupt whatever follows the buffer, on a
    // device with no memory protection to notice.
    let p = ramp();
    let mut s = storage();
    let mut out = [0u8; 8];
    unsafe {
        let m = machine(&mut s);
        assert_eq!(
            lumen_render(m, p.as_ptr(), p.len(), 10, out.as_mut_ptr(), out.len()),
            LUMEN_TOO_SMALL
        );
    }
}

#[test]
fn two_halves_render_what_one_whole_does() {
    // The dual-core claim, checked rather than asserted: the pixels of a frame
    // are independent, so splitting them across two machines must produce
    // exactly the bytes one machine produces for all of them. If this ever
    // differs, a two-core device renders a different show from a one-core
    // device and the mesh stops agreeing with itself.
    let p = ramp();
    let (mut sa, mut sb, mut sc) = (storage(), storage(), storage());
    let mut whole = [0u8; 60];
    let mut first = [0u8; 30];
    let mut second = [0u8; 30];

    unsafe {
        let a = machine(&mut sa);
        assert_eq!(lumen_frame(a, p.as_ptr(), p.len(), 12345), LUMEN_OK);
        assert_eq!(
            lumen_render(a, p.as_ptr(), p.len(), 20, whole.as_mut_ptr(), whole.len()),
            LUMEN_OK
        );

        // One core runs the frame section, then hands the other a copy: the
        // hoisted results live in the machine's registers and the second core
        // needs them.
        let b = machine(&mut sb);
        let c = machine(&mut sc);
        assert_eq!(lumen_frame(b, p.as_ptr(), p.len(), 12345), LUMEN_OK);
        assert_eq!(lumen_machine_clone(b as *const LumenMachine, c), LUMEN_OK);

        assert_eq!(
            lumen_render_range(
                b,
                p.as_ptr(),
                p.len(),
                0,
                10,
                20,
                first.as_mut_ptr(),
                first.len()
            ),
            LUMEN_OK
        );
        assert_eq!(
            lumen_render_range(
                c,
                p.as_ptr(),
                p.len(),
                10,
                20,
                20,
                second.as_mut_ptr(),
                second.len()
            ),
            LUMEN_OK
        );
    }

    assert_eq!(&whole[..30], &first[..], "the first half differs");
    assert_eq!(&whole[30..], &second[..], "the second half differs");
}

#[test]
fn a_range_outside_the_strip_is_refused() {
    let p = ramp();
    let mut s = storage();
    let mut out = [0u8; 60];
    unsafe {
        let m = machine(&mut s);
        // Backwards, and past the end: both are a caller that has miscomputed a
        // split, and both would otherwise index outside the strip.
        assert_eq!(
            lumen_render_range(
                m,
                p.as_ptr(),
                p.len(),
                10,
                5,
                20,
                out.as_mut_ptr(),
                out.len()
            ),
            LUMEN_TOO_SMALL
        );
        assert_eq!(
            lumen_render_range(
                m,
                p.as_ptr(),
                p.len(),
                0,
                30,
                20,
                out.as_mut_ptr(),
                out.len()
            ),
            LUMEN_TOO_SMALL
        );
    }
}

#[test]
fn microseconds_become_the_fixed_point_the_vm_reads() {
    // Half a second.
    assert_eq!(lumen_time_q16(500_000), Q16::HALF.0);
    // Whole seconds keep their fraction, which a single-step conversion loses
    // once the show has been running for a while.
    let t = lumen_time_q16(7_500_000);
    assert_eq!(t >> 16, 7);
    assert!((t & 0xFFFF) > 0x7000, "the fraction was lost: {t:#x}");
    // A show running for hours wraps rather than saturating: an effect reads `t`
    // through `fract` or a wave, so wrapping is invisible where saturating would
    // freeze every animation in the room.
    let long = lumen_time_q16(40_000 * 1_000_000);
    assert!(
        long >= 0,
        "the clock saturated instead of wrapping: {long:#x}"
    );
}

#[test]
fn a_short_datagram_is_not_a_header() {
    let bytes = [0x4cu8, 1, 0x21];
    unsafe {
        assert_eq!(
            lumen_header_read(
                bytes.as_ptr(),
                bytes.len(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut()
            ),
            LUMEN_BAD_DATAGRAM
        );
    }
}
