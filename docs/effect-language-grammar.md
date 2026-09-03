The formal definition of the `.lfx` format. [[Effect Language]] explains the design; this is the specification a parser is written against.

## Lexical structure

```
comment    = "#" , { any-char - newline } ;
ident      = ( letter | "_" ) , { letter | digit | "_" } ;
number     = digit , { digit } , [ "." , { digit } ] , [ unit ] ;
unit       = "m" | "s" | "ms" | "deg" | "rad" | "hz" | "%" ;
hexcolor   = "#" , 6 * hexdigit | "#" , 8 * hexdigit ;
string     = '"' , { any-char - '"' | escape } , '"' ;
```

Whitespace is insignificant. Comments run to end of line. No semicolons; statements end at newline, and an expression may wrap across lines while brackets are open.

**Units are part of the literal, not decoration.** `1.2m` and `1.2` are both `float`, but a `param` declared `unit m` accepts only the former without a warning, and `90deg` converts to radians at parse time. This is cheap to implement and removes an entire category of "why is my rotation 57 times too fast".

## File structure

```
file       = header , { decl } ;
header     = "lumen" , number ;                   (* language version *)
decl       = effect | palette | curve | fn ;
```

A file is self-contained — no imports, no external references ([[Effect Language#Sharing]]). Multiple declarations may share a file; an effect and the palettes it uses travel together.

An **unknown construct is an error**, never skipped. Silent skipping produces effects that render subtly wrong on old software, which is far worse than a refusal to compile.

## Effect

```
effect     = "effect" , string , "{" , { effect-item } , "}" ;

effect-item
           = "version"  , number
           | "author"   , string
           | "stdlib"   , number
           | "requires" , cap , { "," , cap }
           | "fps"      , number                  (* preferred, advisory *)
           | "budget"   , number , "on" , ident   (* optional, CI-enforceable *)
           | param | channel | let | mask | state | layer | sim | fn ;

cap        = "mapped" | "rough" | "rgbw" | "cct" | "audio" | "imu"
           | "grid" | "input" ;
```

`requires mapped` means mapping quality `mapped`; `requires rough` accepts `rough` or better. An effect that declares neither runs on synthetic coordinates too ([[Runtime Model#Unmapped devices mapping is pure upgrade]]).

### Parameters

```
param      = "param" , ident , ":" , type , "=" , expr ,
             [ "range" , expr , ".." , expr ] ,
             [ "unit" , unit ] ,
             [ "step" , expr ] ,
             [ "label" , string ] ;
```

`range` is required for `float` params — it drives the slider in both apps, and a parameter with no bounds cannot be presented in a UI or bound to a MIDI CC. `label` supplies a human name where the identifier is terse.

### Channels

```
channel    = "channel" , ident , ":" , chan-type ,
             [ "hold" , number ] , [ "default" , expr ] ;

chan-type  = "audio_bands" | "audio_beat" | "sim" , "<" , ident , ">"
           | "sensor" , "<" , ident , ">" | "value" | "vec3"
           | "text" , [ "(" , number , ")" ] ;
```

`text` carries a length-prefixed UTF-8 blob, defaulting to a 64-byte maximum. It exists because a scrolling message needs a channel a `value` cannot express — see [[Effect Cookbook]] example 8, which is where the omission was found.

Declaring a channel is what makes an effect *amber* ([[Effects#The green/amber rule]]). `hold` is the staleness window in milliseconds; `default` is what the value decays to when the producer dies — so a dead audio source fades the lights to steady rather than freezing them mid-beat.

**`hold 0` means never stale.** Appropriate for a value that is only pushed on change and stays valid indefinitely — a scrolling message, a mode selector — where treating silence as failure would be wrong. Anything sampled continuously should always set a real window; a channel with no meaningful staleness rule is usually a channel someone forgot to think about.

### Bindings and layers

```
let        = "let" , ident , "=" , expr ;
mask       = "mask" , ident , "=" , expr ;         (* expr must be bool *)
state      = "state" , ident , ":" , type , "=" , expr ;

layer      = "layer" , ident , [ layer-mod ] , "{" , { layer-item } , "}" ;
layer-mod  = { "mask" , "(" , ident , ")" | "blend" , blend | "opacity" , expr } ;
layer-item = assign | let ;
assign     = ident , [ "." , ident ] , "=" , expr ;

blend      = "normal" | "add" | "multiply" | "screen" | "overlay"
           | "max" | "min" | "difference" ;
```

Every layer must assign `color`. Assigning a `state` variable inside a layer writes the per-pixel history buffer.

### Simulations

```
sim        = "sim" , ident , "(" , [ sim-args ] , ")" , "{" , { sim-stmt } , "}" ;
sim-args   = ident , "=" , const-expr , { "," , ident , "=" , const-expr } ;

sim-stmt   = assign | let | if | foreach ;
if         = "if" , expr , "{" , { sim-stmt } , "}" , [ "else" , "{" , { sim-stmt } , "}" ] ;
foreach    = "foreach" , ident , "in" , ident , "{" , { sim-stmt } , "}" ;
```

Sim arguments are **compile-time constants** — `count = 64` sizes an array and cannot be a `param`, because the `sim` VM profile has no dynamic allocation ([[Bytecode VM#Two execution profiles]]).

`if` and `foreach` exist only inside `sim`. The `pixel` profile has no backward branches and no data-dependent control flow; a conditional there is written with `select` or `step`, which is branch-free and keeps the instruction count static so the budget check stays exact.

Sims must be **deterministic**: no free-running randomness, seed from `t` instead. Determinism is what allows replay and sim-master failover.

### Sim accessors

A `sim` block declares an array of records and exposes **accessor functions** to the pixel section — this is how simulation state crosses from the sim master into every device's kernel, and it was missing from the first draft of this grammar.

| Accessor | Returns | Meaning |
|---|---|---|
| `<sim>.influence(p, radius)` | `float` | summed falloff of all elements within `radius` of point `p` |
| `<sim>.nearest(p)` | `float` | distance to the closest element |
| `<sim>.count` | `int` | the declared element count |
| `<sim>.field(p)` | `vec3` | summed vector contribution, for flow and velocity fields |

Accessors are **green**: they run per pixel on every device against its own coordinates, reading the broadcast state as uniforms. Only the `sim` block itself is amber. Element fields are addressed as `p.pos`, `p.vel` and any others the block assigns; every element has the same shape, fixed at compile time.

`influence` is the common case and deserves being a single call rather than a hand-written loop — looping over 64 particles per pixel would be unaffordable, so it compiles to a bounded accumulation with the falloff inlined.

### Functions

```
fn         = "fn" , ident , "(" , [ params ] , ")" , [ "->" , type ] ,
             "{" , { let } , "return" , expr , "}" ;
```

Functions are the text form of an encapsulated node group. They are always inlined — no recursion, no function pointers, no dynamic dispatch.

## Palettes and curves

```
palette    = "palette" , ident , "{" , [ "space" , cspace ] , { stop } , "}" ;
stop       = number , ( hexcolor | color-expr ) ;
cspace     = "oklab" | "oklch" | "hsv" | "linear_rgb" ;

curve      = "curve" , ident , "{" , { number , number } , "}" ;
```

`space` defaults to **oklab**. Stops are resolved at compile time into a lookup table, so the choice of space costs nothing at runtime ([[Effect Language#Colour and palettes]]).

Palettes are referenced by **identifier**, never by string — `palette(warm, x)`, and `param tint : palette = warm`. Strings are only for names, authors and labels. Mixing the two was an inconsistency in the early drafts.

### Stdlib palettes

The standard library ships a small set of named palettes — `warm`, `cool`, `ocean`, `fire`, `ice`, `rainbow`, `mono`, `sunset`. Referencing one is exactly like calling a stdlib function: the stdlib is part of the pinned language version and is vendored into the compiler ([[Tech Stack#Stdlib vendoring]]), so it is **not** an external reference and does not break self-containment ([[Effect Language#Sharing]]).

Without these, every trivial example has to declare its own gradient before it can show anything, which is a poor first five minutes.

## Types

| Type | Representation | Notes |
|---|---|---|
| `float` | `q16` | the default numeric type |
| `int` | `i32` | indices and counts; compile-time in most positions |
| `bool` | `q16` 0 or 1 | so masks and `select` need no separate path |
| `angle` | `q16` radians | literals accept `deg` or `rad`; prevents the classic unit bug |
| `vec2`, `vec3` | 2 or 3 × `q16` | `.x .y .z`, `.u .v` accessors |
| `color` | 3 or 4 × `q16`, linear | always linear; gamma is applied by firmware, never in an effect |
| `palette` | table reference | |
| `curve` | table reference | |

**No implicit narrowing and no silent truncation.** `float` → `angle` requires a unit; `float` → `int` requires `floor`, `round` or `trunc`. `int` → `float` is implicit and always safe.

`color` being *linear* throughout is worth stating loudly: an effect never applies gamma, and never sees a gamma-encoded value. Blending in linear and encoding once at the end is the difference between fades that look right and fades that look cheap ([[Firmware#Colour pipeline]]).

## Built-in variables

Read-only, available per the section the compiler places an expression in.

| Name | Type | Available | Meaning |
|---|---|---|---|
| `t` | `float` | frame, pixel | show time, seconds |
| `dt` | `float` | frame, pixel, sim | seconds since previous frame — readable per pixel, hoisted automatically since it is pixel-invariant |
| `x, y, z` | `float` | pixel | world coordinates, metres |
| `pos` | `vec3` | pixel | the same as a vector |
| `lx, ly, lz` | `float` | pixel | local to the device root |
| `i` | `int` | pixel | LED index within the device |
| `n` | `int` | pixel, frame | LED count on the device |
| `u` | `float` | pixel | 0..1 along the **source's zone projection** |
| `uv` | `vec2` | pixel | 2D, where the zone declares a `grid` projection |
| `prev` | `color` | pixel | this pixel's value last frame |
| `mapq` | `int` | frame | 0 synthetic, 1 rough, 2 mapped |

`u` is a property of the *source*, not the pixel — a pixel covered by three overlapping sources has three values of `u` in one frame ([[Runtime Model#Projections]]). Referencing `uv` without `requires grid` is an error.

## Standard library

**Core — frozen instructions in the [[Bytecode VM]].** Changing this set requires a firmware release, so it stays small.

```
abs ceil clamp floor fract max min mod round sign sqrt trunc
sin cos tan atan2 exp log pow
step smoothstep select mix
length distance dot cross normalize rotate
noise1 noise2 noise3 noise4
palette blend_<mode> hsv oklab temp
vec2 vec3 rgb rgba            (constructors)
```

Constructors are instructions rather than stdlib functions because they appear in nearly every effect and compile to register moves.

**Versioned source library — `stdlib N`, compiled inline.** Grows without a firmware release.

| Group | Functions |
|---|---|
| Easing | `ease_in`, `ease_out`, `ease_in_out`, `bounce`, `elastic`, `back` |
| Waves | `sine01`, `triangle`, `sawtooth`, `square`, `pulse`, `ping_pong` |
| Shapes | `sphere_sdf`, `box_sdf`, `plane_sdf`, `cylinder_sdf`, `torus_sdf` |
| Space | `mirror`, `tile`, `polar`, `spherical`, `twist`, `bend` |
| Noise | `fbm`, `turbulence`, `ridged`, `curl`, `voronoi` |
| Colour | `hue_shift`, `saturate`, `contrast`, `gamma_free_dim`, `kelvin` |
| Random | `hash1`, `hash2`, `hash3`, `rand_per_pixel` — all deterministic from a seed |
| Audio | `band`, `bass`, `mid`, `treble`, `beat_pulse`, `bar_phase` |
| Utility | `remap`, `remap_clamped`, `quantize`, `slew`, `deadzone` |
| Text | `glyph_at`, `text_width`, `char_at` — backed by an embedded 5×7 ASCII bitmap font |

The font lives in the stdlib rather than being referenced externally, because self-contained files admit no external references ([[Effect Language#Sharing]]). It costs about 670 bytes in the constant pool, and only for effects that use it.

`rand_per_pixel` is hash-based, not stateful, so it is stable across frames and identical on every device — which is the only way random-looking effects can stay in sync across a mesh.

## Compile-time diagnostics

Errors:

- unknown identifier, type mismatch, unit mismatch
- `uv` without `requires grid`; `state` inside `sim`; `foreach` outside `sim`
- cycles not passing through `prev` or a channel
- unbounded `param`, or a `sim` argument that is not a constant
- unknown construct, or a `stdlib` version the compiler does not have

Warnings, all worth having early ([[Desktop Application#Debugging effects]]):

- a `let` that could not be hoisted out of `pixel`, and why
- Q16.16 precision loss where a value's range is poorly served by the format
- possible overflow in a multiply chain
- a `mask` positioned so it gates nothing
- a channel declared but never read, or read but never produced
- an effect over budget on a device class in the current mesh

## Open questions

- Should `fn` bodies declare their own `channel` requirements, propagating to callers? Convenient, but it makes bandwidth cost non-local. Allow it and surface the propagation in the editor.
- Is `fps` advisory or binding? Advisory suits an effect that merely prefers a rate; binding would let an effect refuse to run below one. Advisory is probably right, since the frame grid already constrains the options.
`budget n on <class>` is now settled as an optional declaration — it is what [[Effect Cookbook#This note is generated]] enforces in CI, and it lets a shared effect make a machine-checkable claim about what it costs. Ignored entirely for personal effects.

### How a sim accessor reads the sim's elements

**Settled: array reads are legal in the `pixel` profile; writes and length are not.**

The question was which of three ways an accessor could reach the elements, and it
was worth taking slowly because the options differ in what they cost permanently
rather than in what they cost to write.

- **Channel uniforms** were rejected. `CHREAD` addresses a channel with a `u8`
  offset and so reaches 256 bytes, where sixty-four elements' positions alone are
  768 bytes at `q16`. Widening it is a wire-format change, and splitting one sim
  across several channels would make a device's channel budget depend on how a
  simulation happens to be written.
- **Precomputing the field on the sim master** was rejected outright. It works,
  and it breaks the property the whole architecture rests on: what crosses the
  network would then depend on LED count.
- **Array opcodes** were taken, with the smallest change that makes them work.
  `ALOAD` is no longer sim-only, so a pixel kernel may read the broadcast state.
  `ASTORE` and `ALEN` remain sim-only.

Splitting the three array instructions rather than moving all of them is what
keeps this cheap. Only the *sim master* writes, so shared state stays
single-writer with no coordination anywhere. And `ALEN` staying out matters more
than it looks: a per-pixel loop whose trip count came from the array at run time
could not be costed at compile time, and the budget check would stop being exact.
Element count is a compile-time constant, so an accumulation over it unrolls to a
known number of instructions and the compiler can still promise a frame rate.

`OpCode::is_sim_only` and `Program::parse` carry this today, and
`only_writing_and_measuring_an_array_is_sim_only` in `lumen-vm` is the test that
holds it in place.

**What remains is front-end work, not a decision.** `resolve` still refuses `sim`
blocks and `emit` still refuses accessors — loudly and by name, which is the
right failure while it lasts: the grammar and the formatter both carry them, so a
file using one is understood, kept and reformatted correctly, and refused only
where the code would have to exist.
