//! The Lumen bytecode VM.
//!
//! The execution target compiled effects are shipped as. A register machine with
//! 32 registers and Q16.16 fixed-point arithmetic, chosen over native code
//! because one program then runs everywhere — an RP2040 behind a UART bridge
//! executes the same bytes as an ESP32-S3, just slower — and because bounded
//! execution turns "will this run at 60 fps on that device" into a question the
//! compiler answers at publish time rather than one the user discovers as a
//! stutter.
//!
//! # Three sections, three rates
//!
//! | Section | Runs | Sees |
//! |---|---|---|
//! | `once` | on activation | constants, device config |
//! | `frame` | once per frame | show time, channel uniforms, automation |
//! | `pixel` | once per LED | everything above, plus that LED's position and index |
//!
//! The split is the whole performance story: anything the compiler hoists out of
//! `pixel` into `frame` is computed once instead of three hundred times.
//!
//! # Two profiles, one encoding
//!
//! [`Profile::Pixel`] has no backward branches at all — loops are `REPEAT`
//! blocks with a compile-time trip count. [`Profile::Sim`] adds bounded arrays
//! and `FOREACH` over them, and nothing else: no allocation, no recursion, no
//! unbounded anything. Keeping sims in the same bytecode family is what lets a
//! user ship a new simulation as an ordinary effect file, with no firmware
//! release and no privileged position for built-in sims.
//!
//! # Safety by construction
//!
//! No pointers, no syscalls, no unbounded loops, and every instruction charged
//! against a budget. A malicious program can waste its own cycles and nothing
//! else. Everything that could go wrong is a [`Fault`], and a faulting program
//! stops rather than rendering something wrong — the layer above decides what to
//! show instead, which is where "a device is never dark because of software"
//! lives.

#![no_std]
#![forbid(unsafe_code)]

pub mod digest;
pub mod isa;
pub mod noise;
pub mod output;
pub mod program;
pub mod q16;
mod tables;
pub mod vm;

pub use isa::{Instruction, OpCode, Reg};
pub use program::{Program, ProgramError, Section};
pub use q16::Q16;
pub use vm::{Arrays, Machine, NoArrays, PixelInputs, PixelOutput, SliceArrays, Uniforms};

/// Which machine a program was compiled for.
///
/// Enforced at load: a `sim` program must never reach the per-pixel path, where
/// its bounded loops would blow the per-pixel budget.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Profile {
    Pixel,
    Sim,
}

impl Profile {
    pub const fn from_u8(v: u8) -> Option<Profile> {
        Some(match v {
            0 => Profile::Pixel,
            1 => Profile::Sim,
            _ => return None,
        })
    }

    pub const fn to_u8(self) -> u8 {
        self as u8
    }
}

/// Why a program stopped.
///
/// Every one of these is a deterministic outcome, not an accident: given the
/// same program and the same inputs, the same fault occurs at the same
/// instruction on every device. That is what makes a fault reproducible in the
/// simulator instead of a field report about one flickering light.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// Instruction budget for this invocation exhausted.
    ///
    /// The backstop, not the primary defence: the compiler is supposed to prove
    /// the budget before publishing. Reaching this means the estimate was wrong.
    BudgetExceeded,
    /// Malformed, truncated, or otherwise unrunnable bytecode.
    BadProgram,
    /// Out-of-range array access in the `sim` profile.
    OutOfBounds,
    /// Division by zero. No clamped answer could be correct.
    DivideByZero,
    /// A mathematical function was given an input outside its domain — a
    /// negative square root, the log of zero.
    DomainError,
    /// An instruction this VM version does not implement.
    ///
    /// Distinct from [`Fault::BadProgram`] on purpose: it means "upgrade the
    /// firmware", which is a comprehensible thing to tell a user, and the
    /// instruction set being append-only is what keeps it rare.
    UnsupportedInstruction(u8),
    /// A `REPEAT`/`ENDREP` or `CALL`/`RET` nest deeper than the machine allows.
    StackOverflow,
    /// An instruction used a register outside the 32 the machine has.
    BadRegister(u8),
}
