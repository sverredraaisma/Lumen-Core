//! The subcommands.
//!
//! Each returns an [`ExitCode`], and each keeps its real work in a function that
//! takes source text and returns a value — so the logic is testable without
//! touching the filesystem, and the file handling is a thin shell around it.

use std::path::Path;
use std::process::ExitCode;

use lumen_lang::BudgetReport;

use crate::{default_output, read, write_bytes};

/// Exit code for a file the compiler rejected, or a limit that was exceeded.
const REJECTED: u8 = 1;

/// Print diagnostics, and say whether compilation should stop.
fn report(src: &str, diags: &lumen_lang::Diagnostics, quiet: bool) -> bool {
    if diags.is_empty() {
        return false;
    }
    let show = if quiet { diags.has_errors() } else { true };
    if show {
        eprintln!("{}", diags.render(src));
    }
    diags.has_errors()
}

pub fn compile(input: &Path, output: Option<&Path>, quiet: bool) -> ExitCode {
    let src = match read(input) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let (compiled, diags) = lumen_lang::compile(&src);
    let failed = report(&src, &diags, quiet);
    let Some(compiled) = compiled else {
        if !failed {
            eprintln!("lumen: {} produced no program", input.display());
        }
        return ExitCode::from(REJECTED);
    };

    let out = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_output(input));
    if let Err(e) = write_bytes(&out, &compiled.bytecode) {
        return fail(&e);
    }
    if !quiet {
        println!(
            "{} -> {} ({} bytes)",
            input.display(),
            out.display(),
            compiled.bytecode.len()
        );
        print_report(&compiled.report);
    }
    ExitCode::SUCCESS
}

pub fn budget(input: &Path, limit: Option<u32>) -> ExitCode {
    let src = match read(input) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let (compiled, diags) = lumen_lang::compile(&src);
    if report(&src, &diags, false) {
        return ExitCode::from(REJECTED);
    }
    let Some(compiled) = compiled else {
        return ExitCode::from(REJECTED);
    };
    print_report(&compiled.report);

    if let Some(max) = limit {
        let actual = compiled.report.instructions_per_pixel;
        if actual > max {
            // The whole point of `budget` in CI: an effect getting more expensive
            // should be a build failure, so a budget bump is a decision someone
            // makes on purpose.
            eprintln!(
                "lumen: over budget - {actual} per pixel, limit {max} ({} over)",
                actual - max
            );
            return ExitCode::from(REJECTED);
        }
        println!("within budget: {actual} of {max} per pixel");
    }
    ExitCode::SUCCESS
}

pub fn fmt(input: &Path, write: bool, check: bool) -> ExitCode {
    let src = match read(input) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let (formatted, diags) = lumen_lang::format_source(&src);
    if report(&src, &diags, false) {
        return ExitCode::from(REJECTED);
    }
    let Some(formatted) = formatted else {
        return ExitCode::from(REJECTED);
    };

    if check {
        if formatted == src {
            return ExitCode::SUCCESS;
        }
        eprintln!("lumen: {} is not formatted", input.display());
        return ExitCode::from(REJECTED);
    }
    if write {
        if formatted == src {
            return ExitCode::SUCCESS;
        }
        if let Err(e) = write_bytes(input, formatted.as_bytes()) {
            return fail(&e);
        }
        println!("formatted {}", input.display());
        return ExitCode::SUCCESS;
    }
    print!("{formatted}");
    ExitCode::SUCCESS
}

pub fn check(input: &Path) -> ExitCode {
    let src = match read(input) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let (compiled, diags) = lumen_lang::compile(&src);
    if report(&src, &diags, false) {
        return ExitCode::from(REJECTED);
    }
    if compiled.is_none() {
        return ExitCode::from(REJECTED);
    }
    println!("{}: ok", input.display());
    ExitCode::SUCCESS
}

fn print_report(r: &BudgetReport) {
    // The rate the effect asked for, when it asked for one. Advisory: it is what
    // the effect was designed for, not a demand on the device, and a controller
    // choosing a frame grid wants to see it rather than guess.
    if let Some(fps) = r.fps {
        println!("  wants fps : {fps}");
    }
    println!("  per pixel : {}", r.instructions_per_pixel);
    println!("  per frame : {}", r.instructions_per_frame);
    println!("  registers : {} of 32", r.registers_used);
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("lumen: {msg}");
    ExitCode::from(REJECTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "lumen 1\neffect \"x\" {\n  layer b {\n    color = rgb(1, 0, 0)\n  }\n}\n";
    const BAD: &str = "lumen 1\neffect \"x\" {\n  layer b {\n    color = rgb(nope, 0, 0)\n  }\n}\n";

    /// A scratch file that cleans up after itself, so tests do not leave litter
    /// and cannot collide when run in parallel.
    struct Temp(std::path::PathBuf);

    impl Temp {
        fn new(name: &str, contents: &str) -> Temp {
            let mut p = std::env::temp_dir();
            p.push(format!("lumen-cli-test-{}-{name}", std::process::id()));
            std::fs::write(&p, contents).unwrap();
            Temp(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn read(&self) -> String {
            std::fs::read_to_string(&self.0).unwrap()
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(self.0.with_extension("lfxb"));
        }
    }

    fn code(c: ExitCode) -> u8 {
        // ExitCode has no accessor, so compare against the two we produce.
        if format!("{c:?}") == format!("{:?}", ExitCode::SUCCESS) {
            0
        } else {
            1
        }
    }

    #[test]
    fn compiling_a_good_file_writes_bytecode_next_to_it() {
        let t = Temp::new("good.lfx", GOOD);
        assert_eq!(code(compile(t.path(), None, true)), 0);
        let out = t.path().with_extension("lfxb");
        let bytes = std::fs::read(&out).unwrap();
        assert!(!bytes.is_empty());
        // And it really is a program.
        assert!(lumen_vm::program::Program::parse(&bytes).is_ok());
    }

    #[test]
    fn compiling_a_bad_file_fails_and_writes_nothing() {
        let t = Temp::new("bad.lfx", BAD);
        assert_eq!(code(compile(t.path(), None, true)), 1);
        assert!(!t.path().with_extension("lfxb").exists());
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        assert_eq!(code(check(Path::new("does-not-exist.lfx"))), 1);
        assert_eq!(code(compile(Path::new("nope.lfx"), None, true)), 1);
        assert_eq!(code(budget(Path::new("nope.lfx"), None)), 1);
        assert_eq!(code(fmt(Path::new("nope.lfx"), false, false)), 1);
    }

    #[test]
    fn check_accepts_a_good_file_and_rejects_a_bad_one() {
        let good = Temp::new("check-good.lfx", GOOD);
        assert_eq!(code(check(good.path())), 0);
        let bad = Temp::new("check-bad.lfx", BAD);
        assert_eq!(code(check(bad.path())), 1);
    }

    #[test]
    fn budget_enforces_its_limit() {
        // The CI case: an effect getting more expensive must be a build failure.
        let t = Temp::new("budget.lfx", GOOD);
        assert_eq!(code(budget(t.path(), None)), 0);
        assert_eq!(code(budget(t.path(), Some(10_000))), 0);
        assert_eq!(code(budget(t.path(), Some(1))), 1);
    }

    #[test]
    fn fmt_check_passes_on_formatted_input_and_fails_otherwise() {
        let (formatted, _) = lumen_lang::format_source(GOOD);
        let formatted = formatted.unwrap();

        let tidy = Temp::new("tidy.lfx", &formatted);
        assert_eq!(code(fmt(tidy.path(), false, true)), 0);

        let messy = Temp::new(
            "messy.lfx",
            "lumen 1\neffect \"x\"    {\n layer b {\ncolor = rgb(1,0,0)\n}\n}\n",
        );
        assert_eq!(code(fmt(messy.path(), false, true)), 1);
    }

    #[test]
    fn fmt_write_rewrites_the_file_in_place_and_is_idempotent() {
        let t = Temp::new(
            "write.lfx",
            "lumen 1\neffect \"x\"   {\nlayer b {\ncolor = rgb(1,0,0)\n}\n}\n",
        );
        assert_eq!(code(fmt(t.path(), true, false)), 0);
        let once = t.read();
        // A second pass must change nothing, or every save churns the diff.
        assert_eq!(code(fmt(t.path(), true, false)), 0);
        assert_eq!(once, t.read());
        assert_eq!(code(fmt(t.path(), false, true)), 0);
    }

    #[test]
    fn fmt_refuses_a_file_it_cannot_parse() {
        let t = Temp::new("unparseable.lfx", "not an effect file at all\n");
        assert_eq!(code(fmt(t.path(), false, false)), 1);
    }

    #[test]
    fn quiet_suppresses_warnings_but_never_errors() {
        // A warning is advice; an error means the file did not compile, and
        // hiding that would make `-q` dangerous in a script.
        let src = "lumen 1\neffect \"x\" {\n  channel c : value hold 100 default 0\n  layer b { color = rgb(1,0,0) }\n}\n";
        let (_, diags) = lumen_lang::compile(src);
        assert!(
            diags.warnings().count() > 0,
            "expected an unread-channel warning"
        );
        assert!(!diags.has_errors());
        assert!(!report(src, &diags, true), "warnings must not stop a build");

        let (_, bad_diags) = lumen_lang::compile(BAD);
        assert!(report(BAD, &bad_diags, true), "errors must always stop it");
    }
}
