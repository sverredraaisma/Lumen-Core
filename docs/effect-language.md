The canonical form of an effect. The node editor in [[Desktop Application]] is a **view over this text**, not the other way round — the file is the source of truth, and it is the unit people share.

Formal definition in [[Effect Language Grammar]]; worked examples in [[Effect Cookbook]].

## Why an expression language rather than a serialised graph

An expression naturally forms a DAG: `sin(z * 2 - t)` *is* three connected nodes. So a text language and a node graph are two renderings of the same tree, and neither is a lossy export of the other. That gets you diffs, version control, pasteable effects and hand-authoring, without maintaining two models.

The one thing the editor must preserve that the text does not imply is **node layout**. Keep it in a trailing comment block or a sidecar, never in a way that makes the file harder to read or breaks when edited by hand.

## Shape

```
effect "Ocean Sweep" {
  version  1
  author   "sverre"
  stdlib   1
  requires rough

  param speed  : float   = 0.5   range 0..4
  param height : float   = 1.2m  range 0..3  unit m
  param tint   : palette = ocean               # a stdlib palette

  channel audio : audio_bands  hold 400  default 0

  let wave   = sin(z * 2 - t * speed)
  let bright = wave * 0.5 + 0.5 + bass(audio) * 0.3

  mask below = z < height

  layer base {
    color = palette(tint, bright)
  }

  layer sparkle mask(below) blend add {
    color = rgb(1, 1, 1) * step(0.98, noise3(x * 10, y * 10, t))
  }
}
```

Everything green ([[Effects#The green/amber rule]]) is just an expression over `x, y, z, i, u, t`. Everything amber has to be **declared** as a `channel`, which makes the architectural constraint syntactic: you cannot accidentally use audio, you have to ask for it, and the compiler can tell you the bandwidth cost from the declarations alone.

## Elements

| Element | Purpose |
|---|---|
| `param` | user-tweakable, with type, default, range and unit. Drives the UI in both apps automatically |
| `channel` | declares an amber dependency: audio, sim, sensor, external value |
| `let` | a named subexpression, hoisted automatically to `frame` or `once` if it is pixel-invariant |
| `mask` | a boolean expression; attaching it to a layer emits `MASKTEST` with a compile-time skip |
| `layer` | a composite step with a blend mode and optional mask |
| `state` | per-pixel persistent value, backed by the local history buffer |
| `sim` | a shared simulation block; compiles to code that runs on the sim master and broadcasts a channel |
| `fn` | a reusable function, the text form of an encapsulated node group |
| `requires` | capabilities an effect needs: `mapped`, `rough`, `rgbw`, `cct`, `audio`, `imu`, `grid`, `input` |
| `palette` | a named gradient, interpolated in OkLab by default |
| `stdlib` | which standard library version to compile against |

Full syntax in [[Effect Language Grammar]], which is authoritative where this summary is loose.

`requires` is what lets the app say "this effect needs a mapped device" instead of silently rendering something wrong. Prefer `rough` over `mapped` — most spatial effects need approximate positions, not per-LED precision ([[Effect Cookbook#Cookbook conventions]]).

## Stateful example

```
state trail : float = 0

layer comet {
  let head = smoothstep(0.02, 0, abs(u - fract(t * 0.3)))
  trail = max(head, trail * 0.92)      # local history buffer
  color = palette(trail)
}
```

And crossing device boundaries, which needs the sim channel:

```
sim particles(count = 64) {
  foreach p in particles {
    p.vel.z = p.vel.z - 9.81 * dt
    p.pos   = p.pos + p.vel * dt
    if p.pos.z < 0 {
      p.pos.z = 0
      p.vel.z = -p.vel.z * 0.6
    }
  }
}

layer sparks blend add {
  color = rgb(1, 0.75, 0.35) * particles.influence(pos, 0.15m)
}
```

The `sim` block runs once on the sim master; `particles.influence(...)` is green and runs on every device against its own coordinates. The split is visible in the source, which is the point.

`sim` blocks compile to the VM's **`sim` profile** ([[Bytecode VM#Two execution profiles]]) — the same bytecode family with bounded arrays and bounded loops added. That keeps simulations writable by users in ordinary effect files rather than being a fixed set built into firmware, which is what the expandability goal requires. Two constraints follow from the profile: array sizes are declared at compile time (`count = 64` above is a compile-time constant, not a parameter), and a sim must be deterministic, so no free-running randomness — seed it from show time instead.

## Standard library

Two tiers, deliberately.

**Core instructions** — a small set frozen into the [[Bytecode VM]]: `sin`, `cos`, `atan2`, `sqrt`, the noise functions, `palette`, the blend modes, `step`/`smoothstep`, colour space conversion. These are the operations that appear in the inner loop of almost every effect and are worth a table-driven implementation in firmware.

**Versioned source library** — everything else ships as effect-language source that the compiler inlines: easing curves, shapes and distance fields, wave generators, colour helpers, common patterns. It costs instruction budget rather than firmware flash, and it can grow without a firmware release.

An effect declares what it needs:

```
stdlib 2
```

The compiler supplies that version's library. That single line is what makes an effect written today still compile in two years, and it is essential for an open-source project where shared effects outlive the release they were written on. Library versions are additive; a breaking change is a new major version and old versions stay available.

Firmware and stdlib versions are therefore **independent**. A device with old firmware can run an effect using a new stdlib, because the new library compiled down to instructions that device already has. It only fails if the effect needs a genuinely new *instruction* — which is exactly why the core set should stay small and change rarely.

## Colour and palettes

**Palettes interpolate in OkLab by default.** RGB and HSV gradients go muddy or grey through the middle and produce the over-saturated rainbow look that makes LED software recognisable at a glance; a perceptually uniform space simply looks better, and making it the default means the easy path is the good-looking one.

```
palette ocean {
  space oklab            # default; also: hsv, oklch, linear_rgb
  0.0  #001f3f
  0.5  #0074d9
  1.0  #7fdbff
}
```

Other spaces stay available because sometimes the rainbow *is* the intent — an HSV sweep is a legitimate effect, not a mistake. `oklch` is worth offering too: it interpolates hue perceptually, which is what you want for a rainbow that does not have bright yellow and dark blue bands in it.

Interpolation happens at compile time into a lookup table, so the choice of space costs nothing at runtime. Devices then apply their own calibration matrix on top ([[Firmware#Colour pipeline]]), so the same palette looks the same on strips from different batches.

## Toolchain

| Stage | Output |
|---|---|
| Parse | AST, which is the graph |
| Resolve | types, capabilities, channel declarations, cycle check |
| Partition | green kernel vs amber channels |
| Hoist | `once` / `frame` / `pixel` sections |
| Emit | [[Bytecode VM]] program per device class |
| Budget | instructions/pixel × LEDs × fps vs measured capacity score |

A formatter (`fmt`) matters more than usual here: if the node editor round-trips through the formatter, editor output and hand-written files converge on one style and diffs stay clean.

### Where the compiler lives

**Decided: one portable core library, several front-ends.** The whole toolchain — parser, resolver, partitioner, emitter, budget checker — is a single library with no OS dependencies, embedded by:

| Front-end | Why it needs the compiler |
|---|---|
| Desktop editor | live budget bars while patching, publish |
| CLI / headless daemon | CI, scripted publishing, backup and restore |
| Phone app | edit a parameter or a layer without a laptop present |
| Devices with `caps=compile` | the mesh can recompile itself with no app at all |

That last row is the one that makes the autonomy decision honest: a schedule can change an effect, or a device can recompile after a firmware upgrade changes the VM version, with nothing else powered on.

Implications for implementation: it needs to be a language that compiles to native, to a mobile target, and to embedded — Rust or C, `no_std`-friendly, no dynamic allocation on the embedded path. It also needs a memory ceiling low enough for an ESP32-S3, which realistically means the on-device compiler handles small edits and full graphs stay on desktop and phone. Worth measuring early: **compiling a representative effect within a few hundred KB of RAM is the constraint that decides whether `caps=compile` is real.**

## Sharing

An effect is a single self-contained file with its palettes and curves embedded, so a community trades effects by pasting text. That is only true if the format has no external references — **worth defending as a hard rule**, because it is what keeps every future distribution model open.

Suggested: a `.lfx` extension, a header line with the language version, and a strict policy that an unknown construct is an error rather than something silently skipped.

**Decided: files only, for now.** No registry, no server, no infrastructure. Effects are text — people share them by pasting, gisting, or keeping a git repo. Discovery is deliberately somebody else's problem until there is evidence people are actually sharing.

What that decision costs nothing to keep open: as long as a file is self-contained and carries a version, a name and an author in its header, a curated repo or a full registry can be added later without changing the format. So the only obligations now are the self-containment rule above and putting enough metadata in the header that a future index has something to index.

## Open questions

- Do you want types beyond `float`, `vec3`, `color`, `palette`, `bool`? Adding an `angle` type with degree/radian literals would prevent a whole class of confusing effect bugs.
- Should `fn` bodies be allowed to declare their own `channel` requirements, which then propagate to the caller? Convenient, but it makes bandwidth cost non-local. I would allow it and surface the propagation in the editor.
- How should `stdlib` versions be deprecated, if ever? Keeping every version forever is the friendliest policy and costs only repository space, since old versions are just source files.
