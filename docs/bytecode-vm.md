The execution target for compiled effects. Replaces the "native compiled code" idea in the original [[Firmware]] notes.

## Why a VM instead of native code

Native code per chip means shipping an LLVM toolchain inside a phone app, maintaining a backend per architecture, and accepting arbitrary machine code over the network — where a hash proves the transfer was clean but proves nothing about *who* sent it. A VM gives up some speed and buys:

- **One program runs everywhere.** An RP2040 behind a UART bridge executes the same bytes as an ESP32-S3, just slower. Without this, every bridge target needs its own compiler backend.
- **Bounded execution.** The compiler counts instructions, so "will this run at 60 fps on that device" is answered at publish time rather than discovered as a stutter.
- **Safety by construction.** No pointers, no syscalls, no unbounded loops. A malicious program can waste cycles and nothing else.
- **Hot swap without reboot.** A program is data.

Realistic cost: an interpreted per-pixel kernel runs roughly 10–30x slower than native. That still leaves headroom — see the budget table below.

## Machine model

- **Register machine**, 32 registers, not a stack machine. Fewer dispatches per operation, which matters when dispatch dominates.
- **Q16.16 fixed point** throughout. ESP32-S3 has no useful FPU for this workload and the RP2040 has none at all. 16 fractional bits is plenty for colour and position; the compiler inserts scaling.
- **No dynamic allocation, no branches backwards.** Loops are unrolled or expressed as bounded `REPEAT` blocks with a compile-time trip count.
- **Three program sections**, executed at different rates:

| Section | Runs | Sees |
|---|---|---|
| `once` | on activation | constants, device config |
| `frame` | once per frame | show time, CHAN uniforms, automation values |
| `pixel` | once per LED | everything above, plus that LED's position and index |

The split is the whole performance story. Anything the compiler can hoist out of `pixel` into `frame` gets computed once instead of 300 times.

## Per-pixel inputs

| Register | Meaning |
|---|---|
| `x, y, z` | LED world coordinates, metres — synthetic, rough or mapped ([[Runtime Model#Unmapped devices mapping is pure upgrade]]) |
| `lx, ly, lz` | LED coordinates local to the device root |
| `i` | LED index within the device |
| `n` | LED count on the device |
| `u` | normalised 0..1 along the **zone projection of the source being rendered**, not along the strip |
| `uv` | 2D coordinates, where the source's zone declares a `grid` projection |
| `t` | show time, seconds, Q16.16 |
| `prev` | this pixel's value last frame — the local history buffer |

Every LED knowing its own world coordinates is what makes volumetric effects work with zero network traffic: a plane sweeping through a room is `sin(z - t)`, and every device independently gets it right.

`u` deserves care. It is defined by the projection of the zone the *source* targets, so an LED covered by three overlapping sources has three different values of `u` in one frame. Since a program is compiled per (effect, zone) pair, the projection is folded in at compile time and costs nothing at runtime — but it does mean `u` is not a property of the pixel, and the emitter must not cache it as one.

## Instruction set sketch

```
arith    ADD SUB MUL DIV MADD NEG ABS MIN MAX CLAMP
math     SIN COS ATAN2 SQRT POW EXP LOG        (table + interpolation)
noise    NOISE1 NOISE2 NOISE3 NOISE4           (value/simplex, table-driven)
compare  LT LE GT GE EQ SELECT STEP SMOOTHSTEP
space    LEN DIST DOT ROT TRANSFORM
colour   HSV2RGB RGB2HSV PALETTE BLEND_<mode> TEMP2RGB
channel  CHREAD <id> <offset>                  reads a CHAN uniform
history  PREVREAD PREVWRITE
mask     MASKTEST <reg> -> skip N instructions
flow     REPEAT <n> ENDREP  CALL <fn>  RET
array    ALOAD ASTORE ALEN FOREACH      (sim profile only)
debug    PROBE <id> <reg>               (probe builds only)
out      EMIT_RGB EMIT_RGBW EMIT_CCT
```

`SIN`, `NOISE*` and `PALETTE` are table-driven in firmware, not interpreted — they are the hot operations in almost every effect and deserve to be single instructions rather than compiled expressions.

`MASKTEST` is the early-out from [[Effects]]: if the mask register is zero, skip forward N instructions. Because masks are evaluated first and the skip distance is known at compile time, a masked-off pixel costs a handful of instructions instead of a whole layer stack.

`PROBE` exists only in **probe builds** — a program recompiled with instrumentation at the nodes an author is inspecting ([[Desktop Application#Debugging effects]]). It writes a register's value to a small ring buffer the controller reads back, so the editor can compare what the device actually computed against the host simulation. It costs instruction budget like anything else, so probe builds are explicit and bounded, and the compiler must report when instrumentation alone pushed a device over its limit. A normal build contains no `PROBE` at all — debugging must never make the shipped program slower.

## Budgets

The compiler multiplies estimated cycles per pixel by LED count by target fps and compares against the device's **capacity score** — a static number calibrated from a benchmark at boot, meaning "VM instructions per second ÷ 1000". Budget checks must use capacity, never current load, for the reasons in [[Firmware#PPOS capacity not current load]].

Budgets are also per *source*, not per device. A device rendering three overlapping sources runs three kernels per frame, so the check is the **sum over concurrently active sources** on that device — which is why the concurrency limit below matters as much as the per-effect budget.

| Device | LEDs | fps | Instruction budget per pixel |
|---|---|---|---|
| ESP32-S3 @ 240 MHz | 300 | 60 | ~1200 |
| ESP32-C3 @ 160 MHz | 150 | 60 | ~900 |
| RP2040 bridged | 100 | 30 | ~1500 |

The editor should show this live as a budget bar per device, going red before you publish rather than after. Over budget offers three outs: lower fps for that device, simplify the effect, or move the device to bridge-rendered FRAMEs.

## Program format

```
header    magic, VM version, program id, section offsets
constants pool (Q16.16 literals, palettes, curves as tables)
once      section
frame     section
pixel     section
metadata  channel ids consumed, budget estimate, source graph hash
```

Signed and hashed as a unit — see [[Protocol#Security]]. The `source graph hash` lets an editor recognise a program already running on a device and skip the upload.

## Two execution profiles

The per-pixel kernel and a shared simulation need different machines. One instruction encoding, two profiles.

| | `pixel` profile | `sim` profile |
|---|---|---|
| Runs on | every device with `render` | the sim master only |
| Invocation | once per LED per frame | once per frame |
| Memory | registers, the `prev` buffer, and **read-only** access to the broadcast sim arrays | registers plus **bounded arrays** declared at compile time |
| Control flow | no backward branches; `REPEAT` with compile-time trip count | bounded loops over declared arrays, still with a static iteration ceiling |
| Output | pixel colour | a state blob broadcast as a CHAN channel |
| Budget | instructions/pixel × LEDs × fps | instructions × fps, checked against the sim master's capacity |

Extra instructions the `sim` profile adds: array store with a bounds-checked index, an array-length constant, and `FOREACH` over a declared array. Nothing else — no allocation, no recursion, no unbounded anything.

### Why `ALOAD` is legal in both

Array *load* is the one array instruction the `pixel` profile may use. Without it a sim accessor cannot exist: [[Effect Language Grammar#Sim accessors]] says accessors are green — they run per pixel on every device against its own coordinates, reading the broadcast state — and `influence` "compiles to a bounded accumulation with the falloff inlined", which has to read the elements to accumulate over them.

The two alternatives were worse. Addressing the state through `CHREAD` needs a wider channel offset, since a `u8` reaches 256 bytes and sixty-four element positions alone are 768. Accumulating the field on the sim master and broadcasting the result avoids both changes and breaks the property the whole architecture rests on, because what crosses the network would then scale with LED count.

`ASTORE` and `ALEN` stay sim-only, and that asymmetry is the point:

- **No store** means a pixel kernel cannot mutate shared state. Three hundred LEDs on forty devices all writing the same array would need an ordering rule the sans-IO design deliberately does not have, and the result would depend on how the render loop happened to be scheduled.
- **No length** means the trip count of an accumulation is a compile-time constant, so the instruction count stays static and the budget check remains exact. An accumulation whose length was discovered at run time could not be costed before it was published, which is the whole point of the budget.

Loosening the loader is safe in the direction it goes: a device with this rule accepts every program the older rule accepted, so nothing already published stops working. The reverse — an old device meeting a program that reads an array in `pixel` — is what `vm_min_version` is for, and it reports the refusal by name.

Keeping it in the same bytecode family rather than making sims native firmware code is what preserves the expandability promise: **a user can write a new simulation in [[Effect Language]] and ship it as an ordinary effect file**, with no firmware release and no privileged position for built-in sims.

Two requirements this creates:

- **Sims must be deterministic.** Same starting state plus same inputs must give the same result on any device. That is what lets the simulator replay them ([[Desktop Application#Simulator]]), and it is also what makes sim-master failover possible — a new master can resume from a broadcast state snapshot rather than restarting the simulation visibly.
- **Sim state must be serialisable**, since it is both broadcast every frame and handed over on failover. Declared bounded arrays give this for free; it is another reason not to allow dynamic structures.

## Version compatibility

**The instruction set is append-only within a VM major version.** A program declares the minimum VM version it needs; a device refuses any program requiring more than it implements, and reports why.

This resolves an apparent conflict in [[Effect Language]] — that stdlib and firmware versions are independent, yet a firmware upgrade might force a recompile. With append-only instructions, **a firmware upgrade never invalidates a running program**, so the recompile case disappears entirely. New instructions only ever gate *new* effects on *old* devices, which is a comprehensible failure the app can explain and which the small-frozen-core policy keeps rare.

## Open questions

- Do you want **two dialects** — a full one for ESP32-class and a reduced one for tiny bridged nodes — or one dialect where small nodes just fail the budget check? One dialect is far less to maintain; I would start there.
- Should there be an escape hatch for hand-written native effects on devices you control, loaded as a firmware extension rather than over the network? Useful for genuinely expensive effects, and it does not compromise the security model because it goes through flashing, not the protocol.
- Interpreter or a small threaded-code JIT on ESP32-S3? Start interpreted, measure, and only revisit if the budgets above turn out too tight in practice.
