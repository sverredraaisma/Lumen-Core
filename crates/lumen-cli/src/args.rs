//! Hand-written argument parsing.
//!
//! Separated from `main` so it is testable without a process: every case below
//! is exercised by unit tests, which is not true of anything that only runs
//! through `std::env::args`.

use std::path::PathBuf;

pub const USAGE: &str = "\
lumen — compile and inspect Lumen effect files

USAGE:
    lumen <command> [options]

COMMANDS:
    compile <file.lfx> [-o <out.lfxb>]   compile to bytecode
    budget  <file.lfx> [--max <n>]       report cost, optionally enforce a limit
    fmt     <file.lfx> [--write|--check] format canonically
    check   <file.lfx>                   parse and type-check only
    help                                 this text
    version                              versions of the tool, language and protocol

OPTIONS:
    -o, --output <path>   where to write compiled bytecode
        --max <n>         fail if the per-pixel cost exceeds n
    -w, --write           rewrite the file in place
        --check           exit non-zero if the file is not formatted
    -q, --quiet           only report problems

EXIT CODES:
    0  success
    1  the file was rejected, or a limit was exceeded
    2  the command line was wrong
";

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    Help,
    Version,
    Compile {
        input: PathBuf,
        output: Option<PathBuf>,
    },
    Budget {
        input: PathBuf,
        limit: Option<u32>,
    },
    Fmt {
        input: PathBuf,
        write: bool,
        check: bool,
    },
    Check {
        input: PathBuf,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Invocation {
    pub command: Command,
    pub quiet: bool,
}

impl Invocation {
    pub fn parse(argv: &[String]) -> Result<Invocation, String> {
        let mut quiet = false;
        let mut rest: Vec<&str> = Vec::new();
        for a in argv {
            if a == "-q" || a == "--quiet" {
                quiet = true;
            } else {
                rest.push(a.as_str());
            }
        }

        let Some((&verb, tail)) = rest.split_first() else {
            return Ok(Invocation {
                command: Command::Help,
                quiet,
            });
        };

        let command = match verb {
            "help" | "-h" | "--help" => Command::Help,
            "version" | "-V" | "--version" => Command::Version,
            "compile" => {
                let (input, flags) = take_input(tail, "compile")?;
                let mut output = None;
                let mut it = flags.iter();
                while let Some(f) = it.next() {
                    match *f {
                        "-o" | "--output" => {
                            let v = it.next().ok_or("`--output` needs a path")?;
                            output = Some(PathBuf::from(v));
                        }
                        other => return Err(format!("unknown option `{other}` for `compile`")),
                    }
                }
                Command::Compile { input, output }
            }
            "budget" => {
                let (input, flags) = take_input(tail, "budget")?;
                let mut limit = None;
                let mut it = flags.iter();
                while let Some(f) = it.next() {
                    match *f {
                        "--max" => {
                            let v = it.next().ok_or("`--max` needs a number")?;
                            limit = Some(
                                v.parse::<u32>()
                                    .map_err(|_| format!("`--max` needs a number, got `{v}`"))?,
                            );
                        }
                        other => return Err(format!("unknown option `{other}` for `budget`")),
                    }
                }
                Command::Budget { input, limit }
            }
            "fmt" => {
                let (input, flags) = take_input(tail, "fmt")?;
                let mut write = false;
                let mut check = false;
                for f in &flags {
                    match *f {
                        "-w" | "--write" => write = true,
                        "--check" => check = true,
                        other => return Err(format!("unknown option `{other}` for `fmt`")),
                    }
                }
                if write && check {
                    return Err("`--write` and `--check` do opposite things".into());
                }
                Command::Fmt {
                    input,
                    write,
                    check,
                }
            }
            "check" => {
                let (input, flags) = take_input(tail, "check")?;
                if let Some(f) = flags.first() {
                    return Err(format!("unknown option `{f}` for `check`"));
                }
                Command::Check { input }
            }
            other => return Err(format!("unknown command `{other}`")),
        };

        Ok(Invocation { command, quiet })
    }
}

/// Pull the first non-flag argument out as the input path.
///
/// Order-independent so `lumen fmt --write x.lfx` and `lumen fmt x.lfx --write`
/// both work; forcing one order is the kind of thing people hit once and
/// remember as friction.
fn take_input<'a>(tail: &'a [&'a str], verb: &str) -> Result<(PathBuf, Vec<&'a str>), String> {
    let mut input = None;
    let mut flags = Vec::new();
    let mut expecting_value = false;
    for a in tail {
        if expecting_value {
            flags.push(*a);
            expecting_value = false;
            continue;
        }
        if a.starts_with('-') {
            expecting_value = matches!(*a, "-o" | "--output" | "--max");
            flags.push(*a);
        } else if input.is_none() {
            input = Some(PathBuf::from(a));
        } else {
            return Err(format!("`{verb}` takes one file, got two"));
        }
    }
    match input {
        Some(i) => Ok((i, flags)),
        None => Err(format!("`{verb}` needs a file")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Invocation, String> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        Invocation::parse(&owned)
    }

    #[test]
    fn no_arguments_prints_help_rather_than_failing() {
        // A bare `lumen` should teach, not scold.
        assert_eq!(parse(&[]).unwrap().command, Command::Help);
    }

    #[test]
    fn help_and_version_have_the_usual_spellings() {
        for a in ["help", "-h", "--help"] {
            assert_eq!(parse(&[a]).unwrap().command, Command::Help);
        }
        for a in ["version", "-V", "--version"] {
            assert_eq!(parse(&[a]).unwrap().command, Command::Version);
        }
    }

    #[test]
    fn compile_takes_an_input_and_an_optional_output() {
        assert_eq!(
            parse(&["compile", "a.lfx"]).unwrap().command,
            Command::Compile {
                input: "a.lfx".into(),
                output: None
            }
        );
        assert_eq!(
            parse(&["compile", "a.lfx", "-o", "b.lfxb"])
                .unwrap()
                .command,
            Command::Compile {
                input: "a.lfx".into(),
                output: Some("b.lfxb".into())
            }
        );
    }

    #[test]
    fn flags_may_come_before_or_after_the_file() {
        // Forcing one order is the kind of friction people hit once and remember.
        let a = parse(&["fmt", "--write", "x.lfx"]).unwrap().command;
        let b = parse(&["fmt", "x.lfx", "--write"]).unwrap().command;
        assert_eq!(a, b);
    }

    #[test]
    fn budget_parses_its_limit() {
        assert_eq!(
            parse(&["budget", "a.lfx", "--max", "900"]).unwrap().command,
            Command::Budget {
                input: "a.lfx".into(),
                limit: Some(900)
            }
        );
        assert!(parse(&["budget", "a.lfx", "--max", "lots"]).is_err());
        assert!(parse(&["budget", "a.lfx", "--max"]).is_err());
    }

    #[test]
    fn quiet_is_accepted_anywhere() {
        assert!(parse(&["-q", "check", "a.lfx"]).unwrap().quiet);
        assert!(parse(&["check", "a.lfx", "--quiet"]).unwrap().quiet);
        assert!(!parse(&["check", "a.lfx"]).unwrap().quiet);
    }

    #[test]
    fn contradictory_format_flags_are_refused() {
        assert!(parse(&["fmt", "a.lfx", "--write", "--check"]).is_err());
    }

    #[test]
    fn a_missing_file_is_reported_by_name() {
        let e = parse(&["compile"]).unwrap_err();
        assert!(e.contains("needs a file"), "{e}");
    }

    #[test]
    fn two_files_are_refused_rather_than_silently_ignoring_one() {
        let e = parse(&["check", "a.lfx", "b.lfx"]).unwrap_err();
        assert!(e.contains("one file"), "{e}");
    }

    #[test]
    fn unknown_commands_and_options_are_refused() {
        assert!(parse(&["frobnicate"]).is_err());
        assert!(parse(&["check", "a.lfx", "--turbo"]).is_err());
        assert!(parse(&["compile", "a.lfx", "--turbo"]).is_err());
        assert!(parse(&["budget", "a.lfx", "--turbo"]).is_err());
        assert!(parse(&["fmt", "a.lfx", "--turbo"]).is_err());
    }

    #[test]
    fn usage_documents_every_command() {
        for verb in ["compile", "budget", "fmt", "check", "help", "version"] {
            assert!(USAGE.contains(verb), "usage does not mention {verb}");
        }
    }
}
