//! The bytecode VM — skeleton (W3 fills this in).
//!
//! Two profiles over one instruction core:
//!
//! - **`pixel`** — a pure function of position and time, run once per pixel per
//!   frame. Allocation-free, no unbounded loops, budget-checkable at compile
//!   time.
//! - **`sim`** — bounded arrays and bounded loops, for user-written simulations
//!   that publish to a channel. Deterministic by construction.
//!
//! The instruction core is small and **frozen**; capability grows through the
//! versioned source-level standard library instead, so an effect written today
//! still compiles in two years.

#![no_std]
#![forbid(unsafe_code)]

/// Fixed-point scalar: Q16.16. Chosen over floats so a program produces
/// bit-identical output on every chip in the mesh.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Debug)]
pub struct Q16(pub i32);

impl Q16 {
    pub const ZERO: Q16 = Q16(0);
    pub const ONE: Q16 = Q16(1 << 16);

    /// Construct from a whole number of units.
    pub const fn from_int(v: i16) -> Q16 {
        Q16((v as i32) << 16)
    }
}

/// Which profile a program was compiled for. Enforced at load: a `sim` program
/// must never reach the per-pixel path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Profile {
    Pixel,
    Sim,
}

/// Why a program stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// Instruction budget for this pixel exhausted.
    BudgetExceeded,
    /// Malformed or truncated bytecode.
    BadProgram,
    /// Out-of-range array access in the `sim` profile.
    OutOfBounds,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_is_one() {
        assert_eq!(Q16::from_int(1), Q16::ONE);
    }
}
