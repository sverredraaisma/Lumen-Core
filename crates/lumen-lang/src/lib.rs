//! The effect-language compiler — skeleton (W4 fills this in).
//!
//! Text is canonical; the node editor is a view over it. So this crate exposes
//! more than "source in, bytecode out": it also owns the public AST, an edit
//! API the editor drives, and `fmt`, so a round trip through the graph editor
//! leaves a diffable file behind.
//!
//! `alloc` is required — that is what gates on-device `caps=compile`, and
//! whether a representative effect compiles inside a few hundred KB is a
//! measurement the project still owes itself.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

/// Standard-library version an effect is compiled against.
///
/// Versions are additive and old ones never disappear, and the source is
/// vendored by pinned tag — so the same source plus the same stdlib version
/// plus the same compiler yields byte-identical bytecode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StdlibVersion(pub u16);

/// What the compiler reports back alongside the program.
///
/// Not decoration: worst-case cost per pixel and worst-case concurrency per
/// device are what admission control uses, and what turns "this will drop
/// frames" into a compile-time answer rather than a support question.
#[derive(Clone, Copy, Default, Debug)]
pub struct BudgetReport {
    pub instructions_per_pixel: u32,
    pub worst_case_concurrency: u16,
}
