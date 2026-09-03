---
paths:
  - "crates/lumen-vm/**/*.rs"
  - "crates/lumen-hal/**/*.rs"
  - "crates/lumen-proto/**/*.rs"
  - "crates/lumen-crypto/**/*.rs"
---

# `no_std`, no allocation, no floats

These crates run on an ESP32 with no heap available to them. The crate roots
carry `#![no_std]` and `#![forbid(unsafe_code)]`; do not remove either.

- **No `alloc`.** No `Vec`, `String`, `Box`, `HashMap`. Take `&mut [T]` buffers from
  the caller and return lengths. If a design seems to need a heap, it belongs in
  `lumen-lang` or on the caller's side of the boundary.
- **No floating point.** Use `Q16` from `lumen-vm`. A program must produce
  bit-identical output on every chip in the mesh, and `f32` on three different
  targets does not. This is not a style preference.
- **No `std::` anything** — that includes `std::net`, `std::time`, `println!`, and
  `dbg!`. Time and sockets arrive through `lumen-hal` traits.
- Errors are `enum`s that are `Copy`, not boxed trait objects.

`lumen-crypto` is the one crate here with third-party dependencies, and they are
held to the same bar: pure Rust, `no_std`, no allocator. CI builds it for
`riscv32imc-unknown-none-elf`, because a host `cargo test` links `std` through
the test harness and would not notice a dependency quietly pulling it in.

## Testing these crates

`#[cfg(test)]` modules may use `std` (the test harness needs it), so put the
allocation-free assertion in the code and the convenience in the test.

Every public function needs a test. These crates carry the project's numerical
core, and a wrong rounding rule here is invisible until it desynchronises two
devices in a room.
