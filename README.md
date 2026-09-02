# lumen-core

Everything a third-party controller needs in order to talk to a Lumen mesh: the
wire codec, the bytecode VM, the effect-language compiler and the HAL traits.

**Apache-2.0 on purpose.** The boundary in this project is *"how to talk to the
mesh" (open to everyone)* versus *"how to be part of the mesh" (share your
changes)*. Election, replication, the source stack and the render loop are not
here — they live in `lumen-device` under GPL-3.0.

| Crate | Contents |
|---|---|
| `lumen-proto` | wire framing, codec, message types. Hand-written; CI round-trips every vector in `lumen-spec` |
| `lumen-vm` | bytecode VM — `pixel` and `sim` profiles. Allocation-free, Q16.16 |
| `lumen-lang` | compiler: lexer → parser → resolve → partition → hoist → emit, plus the public AST, edit API and `fmt` |
| `lumen-hal` | traits only: `Clock`, `Net`, `Storage`, `LedOut`, … |
| `lumen-cli` | compile / publish / backup over the protocol |

`lumen-vm` and `lumen-hal` are `no_std`. `lumen-lang` needs `alloc`, which is
what gates on-device `caps=compile`.

Published to crates.io on semver so an external controller can depend on it
normally. That is the point of it being permissive.

## Standard library vendoring

Stdlib versions are **vendored by pinned tag** under `stdlib/v1/`, `stdlib/v2/`,
…, never fetched at build time. Builds stay hermetic and offline, which an
embedded toolchain needs — and compilation becomes deterministic: the same
source plus the same stdlib version plus the same compiler produces
byte-identical bytecode. Signed programs are therefore reproducible by anyone
auditing them.

`scripts/vendor-stdlib.sh` pulls from a `lumen-effects` tag and rewrites the
checksum manifest.

## Development

```
cargo test --workspace
```

Cross-repo work (core + device + firmware in one checkout) goes through the
`lumen-dev` meta-repo.
