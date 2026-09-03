//! Drive the real `lumen` binary.
//!
//! `lumen-cli` is a binary crate, so `main` and its command dispatch cannot be
//! called from a test at all — the only way in is to run the executable. That
//! is not a workaround: this file is the CLI's contract as a user meets it, and
//! the exit codes it pins are what CI and every shell script downstream branch
//! on. A command that prints the right thing and returns the wrong code is a
//! broken build that reports success.
//!
//! Exit codes, from `args::USAGE`:
//!   0  success
//!   1  the file was rejected, or a limit was exceeded
//!   2  the command line was wrong

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

/// A well-formed effect, small enough to read but exercising params, a `let`
/// and a layer — so `compile`, `budget` and `check` all have real work to do.
const GOOD: &str = "\
lumen 1

effect \"Cli Fixture\" {
  version 1
  author \"lumen-core\"
  stdlib 1
  fps 60

  param speed : float = 0.15 range 0.02..1 label \"Speed\"

  let phase = sine01(t * speed)

  layer base {
    color = rgb(phase, phase, phase)
  }
}
";

/// Rejected by the compiler: `nope` is not a function.
const BAD: &str = "\
lumen 1

effect \"Broken\" {
  version 1
  author \"lumen-core\"
  stdlib 1
  fps 60

  layer base {
    color = nope(1, 2, 3)
  }
}
";

/// Not even a parse tree: the formatter has nothing to work from.
const UNPARSEABLE: &str = "lumen 1

effect \"Broken\" {
  layer base {
    color = = =
";

// ---- scratch files ---------------------------------------------------------

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A directory of our own, so concurrent tests cannot collide on a filename.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Scratch {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lumen-cli-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch { dir }
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let p = self.dir.join(name);
        std::fs::write(&p, contents).expect("write fixture");
        p
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best effort: a leaked temp dir must never fail a test run.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---- running the binary ----------------------------------------------------

fn lumen(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lumen"))
        .args(args)
        .output()
        .expect("the lumen binary should be runnable")
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("the process exited normally")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("stdout must be UTF-8")
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("stderr must be UTF-8")
}

// ---- help and version ------------------------------------------------------

#[test]
fn help_succeeds_and_lists_every_command() {
    let out = lumen(&["help"]);
    assert_eq!(code(&out), 0);
    let s = stdout(&out);
    for c in ["compile", "budget", "fmt", "check", "help", "version"] {
        assert!(s.contains(c), "usage must document `{c}`:\n{s}");
    }
    assert!(
        s.contains("EXIT CODES:"),
        "usage must document the exit codes"
    );
}

#[test]
fn version_reports_the_tool_the_language_and_the_protocol() {
    // All three matter to someone diagnosing a mesh: the binary alone does not
    // say which language or wire version it will produce.
    let out = lumen(&["version"]);
    assert_eq!(code(&out), 0);
    let s = stdout(&out);
    assert!(s.contains(env!("CARGO_PKG_VERSION")), "stdout: {s}");
    assert!(s.contains("language version"), "stdout: {s}");
    assert!(s.contains("protocol version"), "stdout: {s}");
}

// ---- command-line errors are exit 2 ----------------------------------------

#[test]
fn no_arguments_prints_the_usage_and_succeeds() {
    // Bare `lumen` is a request for help, not a mistake: there is nothing to
    // get wrong yet. A wrong *command* is the error case, and that is exit 2.
    let out = lumen(&[]);
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).contains("USAGE:"), "stdout: {}", stdout(&out));
}

#[test]
fn an_unknown_command_is_a_usage_error() {
    let out = lumen(&["frobnicate", "x.lfx"]);
    assert_eq!(code(&out), 2);
    assert!(
        stderr(&out).starts_with("lumen: "),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn an_unknown_option_is_a_usage_error() {
    let s = Scratch::new();
    let input = s.write("a.lfx", GOOD);
    let out = lumen(&["compile", input.to_str().unwrap(), "--nonsense"]);
    assert_eq!(code(&out), 2);
}

#[test]
fn a_command_with_no_file_is_a_usage_error() {
    for cmd in ["compile", "budget", "fmt", "check"] {
        let out = lumen(&[cmd]);
        assert_eq!(code(&out), 2, "`{cmd}` with no file must be a usage error");
    }
}

// ---- compile ---------------------------------------------------------------

#[test]
fn compile_writes_bytecode_beside_the_source_by_default() {
    let s = Scratch::new();
    let input = s.write("effect.lfx", GOOD);
    let out = lumen(&["compile", input.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));

    let expected = s.path("effect.lfxb");
    let bytes = std::fs::read(&expected).expect("compile must write the default output path");
    assert!(!bytes.is_empty(), "the bytecode must not be empty");
}

#[test]
fn compile_honours_an_explicit_output_path() {
    let s = Scratch::new();
    let input = s.write("effect.lfx", GOOD);
    let out_path = s.path("somewhere-else.bin");
    let out = lumen(&[
        "compile",
        input.to_str().unwrap(),
        "-o",
        out_path.to_str().unwrap(),
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(out_path.exists(), "-o must decide where the bytecode lands");
    assert!(
        !s.path("effect.lfxb").exists(),
        "-o must not also write the default path"
    );
}

#[test]
fn compile_is_deterministic() {
    // Identical source must produce byte-identical bytecode. The "skip the
    // upload if the hash matches" optimisation and reproducible signed programs
    // both rest on this, and a HashMap creeping into the emitter breaks it
    // silently.
    let s = Scratch::new();
    let input = s.write("effect.lfx", GOOD);
    let first = s.path("first.lfxb");
    let second = s.path("second.lfxb");

    for out_path in [&first, &second] {
        let out = lumen(&[
            "compile",
            input.to_str().unwrap(),
            "-o",
            out_path.to_str().unwrap(),
        ]);
        assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    }
    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap(),
        "two compilations of the same source must agree byte for byte"
    );
}

#[test]
fn compile_quiet_says_nothing_on_success() {
    let s = Scratch::new();
    let input = s.write("effect.lfx", GOOD);
    let out = lumen(&["compile", input.to_str().unwrap(), "--quiet"]);
    assert_eq!(code(&out), 0);
    assert!(
        stdout(&out).is_empty(),
        "--quiet must print nothing when there is nothing wrong, got: {}",
        stdout(&out)
    );
}

#[test]
fn compile_rejects_a_bad_effect_with_a_diagnostic() {
    let s = Scratch::new();
    let input = s.write("broken.lfx", BAD);
    let out = lumen(&["compile", input.to_str().unwrap()]);
    assert_eq!(code(&out), 1, "a rejected file exits 1, not 0 and not 2");
    assert!(
        stderr(&out).contains("nope"),
        "the diagnostic must name the offending symbol: {}",
        stderr(&out)
    );
    assert!(
        !s.path("broken.lfxb").exists(),
        "a rejected compile must not leave output behind"
    );
}

#[test]
fn a_missing_input_file_names_the_path_it_could_not_read() {
    let s = Scratch::new();
    let missing = s.path("does-not-exist.lfx");
    for cmd in ["compile", "budget", "fmt", "check"] {
        let out = lumen(&[cmd, missing.to_str().unwrap()]);
        assert_eq!(code(&out), 1, "`{cmd}` on a missing file exits 1");
        assert!(
            stderr(&out).contains("does-not-exist.lfx"),
            "`{cmd}` must name the path it could not read: {}",
            stderr(&out)
        );
    }
}

// ---- budget ----------------------------------------------------------------

#[test]
fn budget_reports_a_cost_and_succeeds_without_a_limit() {
    let s = Scratch::new();
    let input = s.write("effect.lfx", GOOD);
    let out = lumen(&["budget", input.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(!stdout(&out).is_empty(), "budget must report something");
}

#[test]
fn budget_enforces_a_limit_it_is_given() {
    let s = Scratch::new();
    let input = s.write("effect.lfx", GOOD);

    // A limit of one instruction cannot be met by any real effect.
    let tight = lumen(&["budget", input.to_str().unwrap(), "--max", "1"]);
    assert_eq!(code(&tight), 1, "an exceeded budget must fail the command");

    // A limit large enough to be irrelevant must pass, which proves the
    // previous assertion was about the limit and not about `--max` itself.
    let loose = lumen(&["budget", input.to_str().unwrap(), "--max", "1000000"]);
    assert_eq!(code(&loose), 0, "stderr: {}", stderr(&loose));
}

#[test]
fn a_non_numeric_limit_is_a_usage_error() {
    let s = Scratch::new();
    let input = s.write("effect.lfx", GOOD);
    let out = lumen(&["budget", input.to_str().unwrap(), "--max", "lots"]);
    assert_eq!(code(&out), 2);
}

// ---- fmt -------------------------------------------------------------------

/// `fmt` with no flag prints the formatted text and leaves the file alone.
#[test]
fn fmt_prints_to_stdout_and_does_not_touch_the_file() {
    let s = Scratch::new();
    let messy = GOOD.replace("  let phase", "        let phase");
    let input = s.write("messy.lfx", &messy);

    let out = lumen(&["fmt", input.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(!stdout(&out).is_empty());
    assert_eq!(
        std::fs::read_to_string(&input).unwrap(),
        messy,
        "plain `fmt` must not rewrite the file"
    );
}

#[test]
fn fmt_check_passes_on_canonical_source_and_fails_otherwise() {
    let s = Scratch::new();

    // Canonicalise first, so this test asserts the round-trip property rather
    // than a guess about what the formatter's output looks like.
    let input = s.write("effect.lfx", GOOD);
    let canonical = stdout(&lumen(&["fmt", input.to_str().unwrap()]));
    let good = s.write("canonical.lfx", &canonical);
    let out = lumen(&["fmt", good.to_str().unwrap(), "--check"]);
    assert_eq!(code(&out), 0, "canonical source must pass --check");

    let messy = s.write("messy.lfx", &canonical.replace("effect ", "effect  "));
    let out = lumen(&["fmt", messy.to_str().unwrap(), "--check"]);
    assert_eq!(code(&out), 1, "unformatted source must fail --check");
    assert!(
        stderr(&out).contains("is not formatted"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn fmt_write_rewrites_in_place_and_is_idempotent() {
    let s = Scratch::new();
    let input = s.write("effect.lfx", &GOOD.replace("effect ", "effect  "));

    let out = lumen(&["fmt", input.to_str().unwrap(), "--write"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let once = std::fs::read_to_string(&input).unwrap();

    // A second pass must change nothing: "a round trip leaves a file a human
    // would have written" is only true if it also leaves it alone next time.
    let out = lumen(&["fmt", input.to_str().unwrap(), "--write"]);
    assert_eq!(code(&out), 0);
    assert_eq!(std::fs::read_to_string(&input).unwrap(), once);

    let out = lumen(&["fmt", input.to_str().unwrap(), "--check"]);
    assert_eq!(code(&out), 0, "--write must leave the file passing --check");
}

#[test]
fn fmt_formats_a_file_that_parses_even_if_it_will_not_compile() {
    // `fmt` runs on the parse tree, so a file with an unresolved name still
    // formats. That is deliberate: an editor must be able to tidy a file that
    // is mid-edit and does not yet type-check.
    let s = Scratch::new();
    let input = s.write("unresolved.lfx", BAD);
    let out = lumen(&["fmt", input.to_str().unwrap(), "--check"]);
    assert!(
        code(&out) == 0 || code(&out) == 1,
        "--check reports formatting, not semantics; got {}",
        code(&out)
    );
    assert!(
        !stderr(&out).contains("nope"),
        "formatting must not report a name-resolution error: {}",
        stderr(&out)
    );
}

#[test]
fn fmt_refuses_a_file_it_cannot_parse() {
    let s = Scratch::new();
    let input = s.write("broken.lfx", UNPARSEABLE);
    let before = std::fs::read_to_string(&input).unwrap();
    let out = lumen(&["fmt", input.to_str().unwrap(), "--write"]);
    assert_eq!(code(&out), 1);
    assert_eq!(
        std::fs::read_to_string(&input).unwrap(),
        before,
        "a formatter that cannot understand a file must not rewrite it"
    );
}

// ---- check -----------------------------------------------------------------

#[test]
fn check_accepts_a_good_effect_and_writes_no_output_file() {
    let s = Scratch::new();
    let input = s.write("effect.lfx", GOOD);
    let out = lumen(&["check", input.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("ok"), "stdout: {}", stdout(&out));
    assert!(
        !s.path("effect.lfxb").exists(),
        "`check` only parses and type-checks; it must not emit bytecode"
    );
}

#[test]
fn check_rejects_a_bad_effect() {
    let s = Scratch::new();
    let input = s.write("broken.lfx", BAD);
    let out = lumen(&["check", input.to_str().unwrap()]);
    assert_eq!(code(&out), 1);
}

// ---- the corpus ------------------------------------------------------------
//
// There is deliberately no test here that compiles the shipped examples. They
// live in the sibling `lumen-effects` repo, which this repo's CI does not check
// out — `lumen-core` is self-contained on purpose — so such a test would find
// nothing and silently pass on every CI run, which is worse than not having it.
//
// The corpus is covered where it lives: `lumen-effects` CI builds this binary
// from a sibling checkout and runs `lumen budget --max` over every example
// against the ceiling in its manifest.toml, plus `lumen fmt --check` over every
// source. That is the check that fails when a compiler change breaks an effect.
