//! The vendored standard library.
//!
//! Stdlib source is **embedded in the compiler**, not fetched at build time.
//! Two things fall out of that, and both matter more than the inconvenience of
//! having to cut a `lumen-core` release to ship a stdlib change:
//!
//! - Builds are hermetic and work offline, which an embedded toolchain needs.
//! - **Compilation is deterministic.** The same source plus the same stdlib
//!   version plus the same compiler produces byte-identical bytecode, which is
//!   what makes a signed program reproducible by someone auditing it, and what
//!   the "skip the upload if the source hash matches" optimisation silently
//!   depends on.
//!
//! Versions are **additive and old ones never disappear**, so an effect written
//! today still compiles in two years. A new version is a new directory, never an
//! edit to an existing one.
//!
//! The files under `stdlib/vN/` are copies, synchronised from a `lumen-effects`
//! tag by `scripts/vendor-stdlib.sh`. Do not edit them here — change them there
//! and re-vendor, or the two drift and the checksum manifest starts lying.

use alloc::vec::Vec;

use crate::ast::{Decl, FnDecl, Palette};
use crate::diag::{Diagnostic, Diagnostics, Span};
use crate::StdlibVersion;

/// One vendored version: its number and its source files.
struct Version {
    number: u16,
    /// `(file name, source)`. Order is fixed and part of the build, because
    /// declaration order reaches the bytecode.
    files: &'static [(&'static str, &'static str)],
}

/// Every stdlib version this compiler carries.
///
/// Adding a version means adding an entry; never changing one.
static VERSIONS: &[Version] = &[Version {
    number: 1,
    files: &[("core.lfx", include_str!("../../../stdlib/v1/core.lfx"))],
}];

/// The versions this compiler can compile against, lowest first.
pub fn available() -> Vec<StdlibVersion> {
    VERSIONS.iter().map(|v| StdlibVersion(v.number)).collect()
}

/// Whether this compiler carries `version`.
pub fn has(version: StdlibVersion) -> bool {
    VERSIONS.iter().any(|v| v.number == version.0)
}

/// What a stdlib version contributes to an effect's scope.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Stdlib {
    pub fns: Vec<FnDecl>,
    pub palettes: Vec<Palette>,
}

/// Parse a stdlib version.
///
/// A failure here is a compiler bug, not a user error — the source is vendored
/// and was valid when it was vendored — so the diagnostic says so rather than
/// pointing the author at their own file.
pub fn load(version: StdlibVersion, diags: &mut Diagnostics) -> Option<Stdlib> {
    let Some(v) = VERSIONS.iter().find(|v| v.number == version.0) else {
        let known: Vec<u16> = VERSIONS.iter().map(|v| v.number).collect();
        diags.push(Diagnostic::error(
            Span::EMPTY,
            alloc::format!("this compiler does not have stdlib version {}", version.0),
            alloc::format!(
                "it carries {}; update the compiler, or lower the `stdlib` line",
                describe(&known)
            ),
        ));
        return None;
    };

    let mut out = Stdlib::default();
    for (name, src) in v.files {
        let (file, file_diags) = crate::parse(src);
        if file_diags.has_errors() {
            diags.push(Diagnostic::error(
                Span::EMPTY,
                alloc::format!("the vendored stdlib file `{name}` does not parse"),
                "this is a compiler packaging bug, not a problem with your effect; re-vendor the stdlib",
            ));
            return None;
        }
        let Some(file) = file else { return None };
        for d in file.decls {
            match d {
                Decl::Fn(f) => out.fns.push(f),
                Decl::Palette(p) => out.palettes.push(p),
                // An `effect` or `curve` in the stdlib would be silently ignored
                // otherwise, and a silently ignored declaration is how a stdlib
                // ends up shipping something nobody can call.
                _ => {
                    diags.push(Diagnostic::error(
                        Span::EMPTY,
                        alloc::format!(
                            "the vendored stdlib file `{name}` declares something other than a function or a palette"
                        ),
                        "the stdlib may only contain `fn` and `palette` declarations",
                    ));
                    return None;
                }
            }
        }
    }
    Some(out)
}

fn describe(versions: &[u16]) -> alloc::string::String {
    let mut s = alloc::string::String::new();
    for (i, v) in versions.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&alloc::format!("{v}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_one_is_available_and_parses() {
        assert!(has(StdlibVersion(1)));
        assert_eq!(available(), alloc::vec![StdlibVersion(1)]);

        let mut diags = Diagnostics::new();
        let lib = load(StdlibVersion(1), &mut diags).expect("stdlib v1 must parse");
        assert!(!diags.has_errors(), "{:?}", diags.items);
        assert!(!lib.fns.is_empty(), "stdlib v1 declares no functions");
    }

    #[test]
    fn every_available_version_loads() {
        // A version listed but not loadable would fail at the worst moment: on
        // someone else's machine, compiling someone else's effect.
        for v in available() {
            let mut diags = Diagnostics::new();
            assert!(load(v, &mut diags).is_some(), "version {} failed", v.0);
            assert!(!diags.has_errors());
        }
    }

    #[test]
    fn an_unknown_version_says_which_ones_exist() {
        let mut diags = Diagnostics::new();
        assert!(load(StdlibVersion(99), &mut diags).is_none());
        let e = diags.errors().next().unwrap();
        assert!(e.message.contains("99"), "{}", e.message);
        assert!(e.help.contains('1'), "{}", e.help);
    }

    #[test]
    fn stdlib_function_names_are_unique() {
        // Two functions with the same name would make which one you get depend
        // on file order, which is exactly the sort of thing that changes when
        // someone re-vendors.
        let mut diags = Diagnostics::new();
        let lib = load(StdlibVersion(1), &mut diags).unwrap();
        for (i, f) in lib.fns.iter().enumerate() {
            for g in &lib.fns[i + 1..] {
                assert_ne!(f.name, g.name, "duplicate stdlib function `{}`", f.name);
            }
        }
    }
}
