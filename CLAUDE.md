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
| `lumen-vm` | bytecode interpreter, `pixel` and `sim` profiles, and the output stage |
| `lumen-lang` | compiler, plus the public AST, edit API and `fmt` the editor drives |
| `lumen-crypto` | ChaCha20-Poly1305 and Ed25519 behind the `lumen-proto` seam |
| `lumen-capi` | C ABI over the codec and VM, for firmware in other languages |
| `lumen-cli` | `lumen` binary: compile, budget, publish, backup |

Dependencies point one way: `hal` ← `proto`/`vm` ← `lang` ← `cli`, with
`crypto` ← `proto`. Nothing here may depend on `lumen-device`.

## Hard rules

- **Coverage floor is 95%**, measured on the workspace. A change that drops below
  it is not finished. Prefer a test that pins behaviour over one that chases a
  line.
- **Design for extension.** Small crates, narrow public surfaces, traits at the
  seams, phases that run standalone. Every new type gets a plain constructor that
  works without I/O, so it can be built in a test in one line.
- **No `unsafe`.** Crate roots carry `#![forbid(unsafe_code)]`; leave them.
- **No cryptography in `lumen-proto`.** It defines *what* is authenticated —
  which bytes, in what order, under what nonce — and nothing else, so it stays
  dependency-free for third-party controllers. Algorithms live in
  `lumen-crypto`, which is the only crate here allowed third-party
  dependencies, and only pure-Rust `no_std` ones with no allocator.
- **No floating point in shipping code.** `Q16` fixed point, so output is
  bit-identical on every chip in the mesh.
- **There is one output stage, in `lumen-vm::output`, and no gamma in it.** A
  WS2812-class LED's PWM is proportional to emitted light and a Lumen colour is
  already linear light, so an sRGB curve on the way out would make every strip
  brighter than the effect asked for. The problem it gets reached for is
  quantisation - eight bits of linear PWM cannot hold anything below 1/255 - and
  that is what the temporal dithering is for. The dither is deterministic
  because two strips showing one gradient must not shimmer against each other.
- **Nothing from `lumen-device` moves in here**, however convenient. Election,
  replication, the source stack and the render loop are GPL on purpose — see
  `CONTRIBUTING.md`.
- **Never edit a vendored `stdlib/vN/` file directly.** It is a copy of a
  `lumen-effects` tag; change it there and re-vendor.

## Firmware in other languages

`crates/lumen-capi` is the codec and the VM behind a C ABI, and
`nodes/esp8266/` builds it as a static library for the ESP8266 — a chip whose
radio has no Rust driver and will not get one cheaply. The firmware keeps WiFi
and the LED output in whatever its SDK speaks; linking this keeps *rendering*
bit-identical with the rest of the mesh, which is the reason the VM is fixed
point and precisely what a second implementation would lose.

`nodes/esp8266` is outside the workspace: it cross-compiles to a bare Xtensa
target and carries its own panic handler. `lumen-capi` itself is an `rlib` in the
workspace and is tested there — a `#[panic_handler]` cannot coexist with the one
std brings to a test binary, which is why the two are separate crates.

## Gotchas

> Living section. Add anything that cost real time.

- **The "cannot link / no local coverage" note used to be wrong; both now work.**
  `link.exe` was never missing. What was missing was the **Windows SDK**, so the
  linker had no `kernel32.lib` to link against and Rust reported that as
  "linker `link.exe` not found". Adding the SDK component to the existing VS
  2022 install fixed the MSVC toolchain and `cargo llvm-cov` together. If a
  fresh machine shows this symptom, install the C++ workload rather than
  switching to `windows-gnu`: that workaround builds, which is why nobody
  revisits it, and it silently costs you coverage because the `windows-gnu`
  toolchain ships no profiler runtime.
- **The C ABI is tested from Rust, through raw pointers, including the ways C
  gets it wrong.** A null, a short buffer, misaligned storage, a backwards range:
  each has a test. There is no borrow checker on the far side of that boundary
  and no second chance on a device in somebody's ceiling.
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
