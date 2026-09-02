//! The Lumen effect-language compiler.
//!
//! Text is canonical and the node editor is a view over it, so this crate is
//! more than "source in, bytecode out": it also owns the public [`ast`], which
//! the editor builds and mutates directly, and [`fmt`], which writes a diffable
//! file back out. A round trip through the graph editor has to leave a file a
//! human would have written.
//!
//! # Phases
//!
//! ```text
//! source -> lex -> parse -> resolve -> emit -> bytecode + BudgetReport
//! ```
//!
//! Each phase is separately callable with a plain input and a plain output, so a
//! test can drive one without standing up the others.
//!
//! # Two properties worth defending
//!
//! **Determinism.** Identical source plus identical stdlib version plus
//! identical compiler must produce byte-identical bytecode. Reproducible signed
//! programs depend on it, and so does the "skip the upload if the source hash
//! matches" optimisation. Nothing here may iterate a hash map and emit in
//! iteration order, or read a clock, an environment variable or a path.
//!
//! **Diagnostics are a product surface.** Every error carries a span and a help
//! line saying what to do. See [`diag`].

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

pub mod ast;
pub mod diag;
pub mod emit;
pub mod fmt;
pub mod lex;
pub mod parse;
pub mod resolve;
pub mod stdlib;

pub use diag::{Diagnostic, Diagnostics, Severity, Span};
pub use parse::parse;

/// Standard-library version an effect is compiled against.
///
/// Versions are additive and old ones never disappear, and the source is
/// vendored by pinned tag — so the same source plus the same stdlib version plus
/// the same compiler yields byte-identical bytecode.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct StdlibVersion(pub u16);

/// The stdlib version assumed when an effect does not name one.
pub const DEFAULT_STDLIB: StdlibVersion = StdlibVersion(1);

/// The language version this compiler implements, as it appears in the `lumen N`
/// header.
pub const LANGUAGE_VERSION: u32 = 1;

/// What the compiler reports back alongside the program.
///
/// Not decoration: worst-case cost per pixel and worst-case concurrency per
/// device are what admission control uses, and what turns "this will drop
/// frames" into a compile-time answer rather than a support question.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct BudgetReport {
    /// Cost of the `pixel` section, in [`lumen_vm::OpCode::cost`] units.
    pub instructions_per_pixel: u32,
    /// Cost of the `frame` section, paid once per frame however many LEDs there
    /// are. The whole point of hoisting is to move work here.
    pub instructions_per_frame: u32,
    /// Cost of the `once` section.
    pub instructions_once: u32,
    /// Registers the program needs live at its widest point.
    pub registers_used: u8,
}

/// Compile source text to bytecode.
///
/// The whole pipeline in one call: lex, parse, resolve, emit. Returns the
/// compiled program when there were no errors, alongside every diagnostic —
/// warnings are returned on success too, because a program that compiles with a
/// "this could not be hoisted" warning is exactly the case an author needs to
/// see.
pub fn compile(src: &str) -> (Option<emit::Compiled>, Diagnostics) {
    let (file, mut diags) = parse(src);
    let Some(mut file) = file else {
        return (None, diags);
    };
    if !link_stdlib(&mut file, &mut diags) {
        return (None, diags);
    }
    let Some(resolved) = resolve::resolve(&file, &mut diags) else {
        return (None, diags);
    };
    let compiled = emit::emit(&resolved, &mut diags);
    (compiled, diags)
}

/// Bring the declared stdlib version into the file's own scope.
///
/// The stdlib is **compiled inline**, exactly like a function the author wrote,
/// which is what keeps a `.lfx` file self-contained: referencing `ease_out` is
/// not an external reference, because the definition is vendored into the
/// compiler and is part of the pinned language version.
///
/// Unused functions cost nothing. The emitter inlines on demand, so a file that
/// calls two stdlib functions carries two, not the whole library.
fn link_stdlib(file: &mut ast::File, diags: &mut Diagnostics) -> bool {
    let Some(effect) = file.decls.iter().find_map(|d| match d {
        ast::Decl::Effect(e) => Some(e),
        _ => None,
    }) else {
        // No effect is a problem, but it is `resolve`'s to report, with a better
        // message than anything this function could give.
        return true;
    };
    let version = effect
        .stdlib
        .map(|v| StdlibVersion(v as u16))
        .unwrap_or(DEFAULT_STDLIB);

    let Some(lib) = stdlib::load(version, diags) else {
        return false;
    };

    // Palettes first, so a stdlib palette is visible to the effect that
    // follows. Spans cleared for the same reason as the functions: a library
    // span points into a file the author cannot see, and `resolve` recognises
    // the empty span when it explains a name clash.
    for mut p in lib.palettes {
        p.span = Span::EMPTY;
        file.decls.push(ast::Decl::Palette(p));
    }
    // A file-scope `fn` is a top-level declaration in the grammar, but only
    // effect-item functions were ever registered - so calling one reported
    // "unknown function" at the call site while the declaration itself was
    // accepted in silence. A declaration that parses and then does nothing is
    // the exact failure the "unknown construct is an error" rule exists to
    // prevent, so they are folded in here alongside the library.
    let top_level: Vec<ast::FnDecl> = file
        .decls
        .iter()
        .filter_map(|d| match d {
            ast::Decl::Fn(f) => Some(f.clone()),
            _ => None,
        })
        .collect();

    for d in &mut file.decls {
        if let ast::Decl::Effect(e) = d {
            // Prepended, so the library is declared first and a clashing user
            // function is the one reported — at a span inside the file the
            // author is actually looking at.
            //
            // Library spans are cleared for the same reason: they point into a
            // different file, and rendering one against the user's source would
            // put a caret at an arbitrary offset. An empty span is how
            // `resolve` recognises a stdlib declaration when it explains the
            // clash.
            let mut fns: Vec<ast::FnDecl> = lib
                .fns
                .iter()
                .cloned()
                .map(|mut f| {
                    f.span = Span::EMPTY;
                    f
                })
                .collect();
            fns.extend(top_level.iter().cloned());
            fns.append(&mut e.fns);
            e.fns = fns;
            break;
        }
    }
    true
}

/// Parse and reformat, for the editor's round trip.
pub fn format_source(src: &str) -> (Option<alloc::string::String>, Diagnostics) {
    let (file, diags) = parse(src);
    (file.as_ref().map(fmt::format), diags)
}
