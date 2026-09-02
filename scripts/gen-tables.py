#!/usr/bin/env python3
"""Regenerate crates/lumen-vm/src/tables.rs.

The VM has no float unit to build these at startup with, and a table that is
identical bytes on every target is what makes SIN produce the same pixel on an
ESP32-C3 and in the simulator.

Changing a table changes rendered output, so it is a VM major version decision
rather than a bug fix. Run from the repo root:

    python3 scripts/gen-tables.py > crates/lumen-vm/src/tables.rs
"""

import math

ONE = 65536

HEADER = """//! Generated lookup tables for the transcendental instructions.
//!
//! Checked in rather than computed at startup: the VM has no float unit to
//! build them with, and a table that is identical bytes on every target is what
//! makes `SIN` produce the same pixel on an ESP32-C3 and in the simulator.
//!
//! Regenerate with `scripts/gen-tables.py` if a table ever needs to change —
//! but note that changing one changes rendered output, so it is a VM major
//! version decision, not a bug fix.
"""


def emit(name, doc, vals, per_line=8):
    print(f"/// {doc}")
    print(f"pub(crate) static {name}: [i32; {len(vals)}] = [")
    for i in range(0, len(vals), per_line):
        row = ", ".join(str(v) for v in vals[i : i + per_line])
        print(f"    {row},")
    print("];\n")


def main():
    print(HEADER)

    sin_q = [int(round(math.sin(2 * math.pi * i / 1024) * ONE)) for i in range(257)]
    sin_q[256] = ONE
    emit(
        "SIN_QUARTER",
        "sin(2*pi*i/1024) in Q16, for i in 0..=256. One quarter period; symmetry gives the rest.",
        sin_q,
    )

    log2_m = [int(round(math.log2(1 + i / 256) * ONE)) for i in range(257)]
    log2_m[256] = ONE
    emit("LOG2_MANTISSA", "log2(1 + i/256) in Q16, for i in 0..=256. Range 0..1.", log2_m)

    exp2_f = [int(round((2 ** (i / 256) - 1) * ONE)) for i in range(257)]
    exp2_f[256] = ONE
    emit("EXP2_FRACTION", "2^(i/256) - 1 in Q16, for i in 0..=256. Range 0..1.", exp2_f)

    atan_t = [int(round(math.atan(i / 256) * ONE)) for i in range(257)]
    emit("ATAN_TABLE", "atan(i/256) in Q16 radians, for i in 0..=256. Range 0..pi/4.", atan_t)


if __name__ == "__main__":
    main()
