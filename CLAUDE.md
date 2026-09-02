# lumen-core

The permissive half of an ARGB mesh lighting system: wire codec, bytecode VM,
effect-language compiler, HAL traits, CLI. Everything a third-party controller
needs to *talk to* a mesh — and nothing that makes a device a device.

- **Licence:** Apache-2.0. Mesh state machines are GPL and live in `lumen-device`.
- **Main branch:** `main`
- **Status:** skeleton. W1 (foundations) done; W2/W3/W4 fill in the crates.

## Stack

- Rust 1.85+, edition 2021
- `lumen-vm`, `lumen-hal`, `lumen-proto`: `no_std`, no `alloc`
- `lumen-lang`: `no_std` + `alloc`
- No async runtime anywhere in this repo

## Commands

```bash
cargo test --workspace
cargo test -p lumen-vm                       # one crate
cargo clippy --workspace --all-targets       # CI runs with -D warnings
cargo fmt --all -- --check                   # dry run
cargo fmt --all                              # fix
cargo llvm-cov --workspace --summary-only    # coverage; must be >= 95%
```

## Layout

| Crate | Rule |
|---|---|
| `lumen-hal` | traits only, zero implementations |
| `lumen-proto` | wire framing and codec. Hand-written; round-trips `lumen-spec` vectors |
| `lumen-vm` | bytecode interpreter, `pixel` and `sim` profiles |
| `lumen-lang` | compiler, plus the public AST, edit API and `fmt` the editor drives |
| `lumen-cli` | `lumen` binary: compile, budget, publish, backup |

Dependencies point one way: `hal` ← `proto`/`vm` ← `lang` ← `cli`. Nothing here
may depend on `lumen-device`.

## Hard rules

- **Coverage floor is 95%**, measured on the workspace. A change that drops below
  it is not finished. Prefer a test that pins behaviour over one that chases a
  line.
- **Design for extension.** Small crates, narrow public surfaces, traits at the
  seams, phases that run standalone. Every new type gets a plain constructor that
  works without I/O, so it can be built in a test in one line.
- **No `unsafe`.** Crate roots carry `#![forbid(unsafe_code)]`; leave them.
- **No floating point in shipping code.** `Q16` fixed point, so output is
  bit-identical on every chip in the mesh.
- **Nothing from `lumen-device` moves in here**, however convenient. Election,
  replication, the source stack and the render loop are GPL on purpose — see
  `CONTRIBUTING.md`.
- **Never edit a vendored `stdlib/vN/` file directly.** It is a copy of a
  `lumen-effects` tag; change it there and re-vendor.

## Gotchas

> Living section. Add anything that cost real time.

- **Local coverage does not work on this machine.** The `windows-gnu` toolchain
  ships no profiler runtime, so `cargo llvm-cov` fails with "the compiler may have
  been built without the profiler runtime". The gate that counts runs in CI on
  Linux. Installing the VS Build Tools C++ workload fixes this and the linker.
- **The default toolchain on this machine cannot link.**
  `stable-x86_64-pc-windows-msvc` is the default but MSVC's `link.exe` is not
  installed, so every build dies at the link step with "linker `link.exe` not
  found". Use `cargo +stable-x86_64-pc-windows-gnu ...`, or install the VS Build
  Tools C++ workload. This is environment-specific, not a repo problem.
- **Git Bash shadows MSVC's linker.** Even with MSVC installed, `/usr/bin/link`
  (GNU coreutils) precedes it on PATH inside Git Bash and fails with
  "link: missing operand". Build from PowerShell, or use the gnu toolchain.
- **`HashMap` iteration order breaks reproducible builds.** The compiler must emit
  byte-identical bytecode for identical input; iterate a `BTreeMap`, or sort
  before emitting.

## Specialized guides (loaded on demand — do not preload)

- `no_std` crate constraints: `.claude/rules/no-std-crates.md` (auto-loads on those files)
- Compiler-specific rules: `.claude/rules/compiler-crate.md` (auto-loads on those files)
- Design notes: `docs/bytecode-vm.md`, `docs/effect-language.md`,
  `docs/effect-language-grammar.md` — read the one you need, they are long
- Licence boundary and the four cross-cutting design rules: `CONTRIBUTING.md`

## Compact instructions

Preserve code changes, file paths touched, decisions made, and any measured
number (coverage, budget, RAM). Drop raw build and test output.
