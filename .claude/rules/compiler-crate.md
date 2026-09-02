---
paths:
  - "crates/lumen-lang/**/*.rs"
---

# The compiler crate

`lumen-lang` is `#![no_std]` with `extern crate alloc`. It may allocate; it may not
touch `std`.

That combination is not decoration: `caps=compile` means this crate has to compile
a representative effect inside a few hundred KB of ESP32 RAM. Every `Vec` that
grows with program size is a claim against that budget, and whether the budget
holds is still an open measurement.

## Determinism is a hard requirement

Identical source + identical stdlib version + identical compiler must produce
**byte-identical bytecode**. Two things silently break that, so watch for both:

- Iterating a `HashMap`/`HashSet` and emitting in iteration order. Use `BTreeMap`,
  or sort before emitting.
- Anything that reads the clock, the environment, or a path.

The "skip the upload if the source hash matches" optimisation and reproducible
signed programs both rest on this.

## Structure

Keep the phases separable — lexer → parser → resolver → partitioner → hoister →
emitter — each testable on its own with a plain input and a plain output. The
editor drives the public AST and `fmt` directly, so those are API, not internals.

## Diagnostics are a product surface

An error message is user experience. Every rejection needs a test in
`lumen-effects/examples/failing/` asserting the *specific* diagnostic, not just
that compilation failed.
