# Vendored standard library

Stdlib source is copied in from a `lumen-effects` tag, one directory per
version, with a checksum manifest. Nothing is fetched at build time.

- Builds are hermetic and work offline, which an embedded toolchain needs.
- Compilation is deterministic, so identical source compiles to identical
  bytecode and a signed program is reproducible by an auditor.
- The cost: a stdlib release needs a `lumen-core` release to reach users.
  Acceptable, because stdlib versions are additive and old ones never disappear.

Update with `scripts/vendor-stdlib.sh <lumen-effects-tag>`.
