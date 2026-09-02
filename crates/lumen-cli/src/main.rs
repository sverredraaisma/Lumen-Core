//! `lumen` — compile, budget, format, check.
//!
//! The CLI exists before any GUI on purpose: the compiler is the product and the
//! editor is the convenience, and this is what CI runs. It is also the reference
//! for how a third-party tool drives the compiler, which is why it is Apache
//! licensed and restricts itself to compiling and publishing *over the
//! protocol*. The moment it wants to join the mesh as a participant it links
//! `lumen-device` and belongs on the GPL side.
//!
//! Argument parsing is hand-written. A dependency-free binary is worth more here
//! than the convenience: `lumen-cli` is what someone builds first to check the
//! project works, and every crate it pulls in is another thing that can fail on
//! their machine.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod args;
mod cmd;

use args::{Command, Invocation};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match Invocation::parse(&argv) {
        Ok(inv) => run(inv),
        Err(msg) => {
            eprintln!("lumen: {msg}\n");
            eprint!("{}", args::USAGE);
            ExitCode::from(2)
        }
    }
}

fn run(inv: Invocation) -> ExitCode {
    match inv.command {
        Command::Help => {
            print!("{}", args::USAGE);
            ExitCode::SUCCESS
        }
        Command::Version => {
            println!("lumen {}", env!("CARGO_PKG_VERSION"));
            println!("language version {}", lumen_lang::LANGUAGE_VERSION);
            println!("protocol version {:#04x}", lumen_proto::PROTOCOL_VERSION);
            ExitCode::SUCCESS
        }
        Command::Compile { input, output } => cmd::compile(&input, output.as_deref(), inv.quiet),
        Command::Budget { input, limit } => cmd::budget(&input, limit),
        Command::Fmt {
            input,
            write,
            check,
        } => cmd::fmt(&input, write, check),
        Command::Check { input } => cmd::check(&input),
    }
}

/// Read a file, reporting the path in the error rather than just "not found".
pub fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

pub fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The default output path for a compiled effect: the input with `.lfxb`.
pub fn default_output(input: &Path) -> PathBuf {
    input.with_extension("lfxb")
}
