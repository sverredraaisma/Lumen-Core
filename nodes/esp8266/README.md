# A Lumen node on an ESP8266

The ESP-01 is the cheapest WiFi microcontroller anybody can buy, and what decides
whether a lighting system spreads through a house is what the twentieth node
costs. This is how one joins the mesh.

## What this is, and what it is not

**Rust is not translated to C.** `rustc` targets `xtensa-esp8266-none-elf`
directly, so the portable half of Lumen compiles to ESP8266 machine code in an
ordinary static library. Your firmware — Arduino core, NONOS SDK, whatever you
already use — links `liblumen_esp8266.a` and calls into it with plain C
function calls. There is no generated C, no translation step, and no cost at the
boundary beyond a function call.

The split is:

| | who |
|---|---|
| WiFi, sockets, DHCP | **your firmware**, using the SDK it already has |
| driving the LEDs | **your firmware** |
| reading a datagram's header | this library |
| running a compiled effect | this library |

That division is not a compromise. The ESP8266's radio is a closed blob from the
NONOS SDK with no Rust binding, and writing one is months of work
([`lumen-dev/docs/esp8266.md`](../../../lumen-dev/docs/esp8266.md) costs it out).
But the radio is also the part that does *not* need to agree with the rest of the
mesh. Rendering is, and linking this keeps it **bit-identical** with every other
device — which is the entire reason the VM is fixed point, and exactly what a
second implementation in C would lose.

## Size

Measured on the built archive:

- **23.6 KB** of Lumen code.
- The archive also carries `compiler_builtins` at 198 KB, of which the linker
  takes only the few 64-bit helpers actually referenced.

Comfortable on an ESP-01's 1 MB, and on a 512 KB one.

## Building it

Needs the Espressif Rust fork, which carries the Xtensa backend:

```bash
cargo install espup --locked
espup install
# Windows: . $HOME/export-esp.ps1     Linux/macOS: . $HOME/export-esp.sh

cd lumen-core/nodes/esp8266
rustup run esp cargo build --release
# -> target/xtensa-esp8266-none-elf/release/liblumen_esp8266.a
```

Then point your firmware at that `.a` and at `include/lumen.h`. For PlatformIO:

```ini
[env:esp01]
platform = espressif8266
board = esp01_1m
framework = arduino
build_flags = -I ${PROJECT_DIR}/lumen/include
board_build.f_cpu = 160000000L        ; the VM likes the boost, see below
extra_scripts = post:link_lumen.py    ; adds the .a to LINKFLAGS
```

## Using it

The whole of a rendering node, minus the networking you already have:

```c
#include "lumen.h"

#define LEDS 60

/* Storage the library builds its machine in. uint64_t so it is aligned. */
static uint64_t machine_backing[64];
static LumenMachine *machine;

/* The program a controller pushed, and its rendered output. */
static uint8_t program[2048];
static size_t  program_len;

/* Linear light, then the codes the strip consumes, then the dither state that
 * carries between frames. Three buffers because rendering and encoding are
 * separate jobs; at 60 LEDs that is 720 + 180 + 720 bytes. */
static int32_t linear[LEDS * LUMEN_BYTES_PER_PIXEL];
static uint8_t pixels[LEDS * LUMEN_BYTES_PER_PIXEL];
static int32_t residual[LEDS * LUMEN_BYTES_PER_PIXEL];

static LumenOutput output = {
    .brightness_q16 = 0,      /* full */
    .budget_ma      = 500,    /* what a USB port promises without asking */
    .residual       = residual,
};

void lumen_panic(void) {
    os_printf("lumen: panic\n");
    system_restart();
    while (1) {}
}

void setup_lumen(void) {
    if (lumen_machine_init(machine_backing, sizeof machine_backing, &machine) != LUMEN_OK) {
        os_printf("lumen: machine storage too small\n");
    }
}

/* A datagram arrived on the socket your firmware owns. */
void on_datagram(const uint8_t *bytes, size_t len) {
    uint8_t  type;
    uint16_t mesh;
    uint64_t show_time;
    if (lumen_header_read(bytes, len, &type, &mesh, &show_time) != LUMEN_OK) {
        return;                      /* not ours; drop it in silence */
    }
    if (mesh != MY_MESH_PREFIX) {
        return;                      /* somebody else's mesh on the same LAN */
    }
    /* ... your handling: a program to store, a channel value to keep ... */
}

/* Called from your frame timer. `now_us` is your show clock. */
void render(uint64_t now_us) {
    if (program_len == 0) {
        return;
    }
    int32_t t = lumen_time_q16(now_us);

    if (lumen_frame(machine, program, program_len, t) != LUMEN_OK) {
        return;                      /* hold the last frame rather than flash */
    }
    if (lumen_render(machine, program, program_len,
                     LEDS, linear, sizeof linear) != LUMEN_OK) {
        return;
    }

    uint32_t draw_ua = 0;
    if (lumen_encode(linear, LEDS, pixels, sizeof pixels,
                     &output, &draw_ua, NULL) != LUMEN_OK) {
        return;
    }
    strip_write(pixels, sizeof pixels);   /* your LED driver */
}
```

Two things worth copying from that:

**Hold the last frame on failure.** Both error paths return without touching
`pixels`, so a program that faults leaves the strip showing what it showed
before. Blanking would be a visible flash on every glitch, and "a device is
never dark because of software" is one of the four rules this project checks
every change against.

**One call for the strip.** There is no per-pixel entry point on purpose. The
same shape measured on this project's Android binding cost over two hundred
times what the batched call did; this boundary is far cheaper than that one, and
it is still the wrong place for a loop.

## What it can drive

Extrapolated from the ESP32-C3's measured 134 cycles per VM instruction — the
interpreter is dispatch-bound, so cycles per instruction ports across chips
better than a wall-clock figure. Assumes 60% of the frame for rendering, the
same split the main budget table uses.

| | instructions per pixel available |
|---|---|
| 60 LEDs @ 30 fps, 80 MHz | 133–199 |
| 60 LEDs @ 30 fps, 160 MHz boost | 265 |
| 150 LEDs @ 30 fps, 80 MHz | 53–80 |

The shipped corpus needs **12 to 41**, so 60 LEDs at 30 fps is comfortable and
150 is workable. Run at 160 MHz if the board allows it; the VM is dispatch-bound
and takes the clock straight.

**These are extrapolations, not measurements.** The number to measure first is
cycles per instruction on real ESP8266 silicon, which needs only this library
and a timer — no radio, no network.

## Memory

The tighter constraint, at roughly 40 KB of usable RAM once the WiFi stack has
taken its share:

| | bytes |
|---|---|
| machine storage | `lumen_machine_size()`, a few hundred |
| a compiled program | 1–3 KB, whatever the controller pushed |
| 60 LEDs of linear light | 720 |
| 60 LEDs of dither state | 720 |
| 60 LEDs of output | 180 |

The dither state is optional — pass `NULL` and the library rounds instead — but
it is the cheapest 720 bytes here. Without it nothing below 1/255 reaches the
strip, so every fade ends in a few visible steps well before it reaches black.

That fits with room to spare. A strip long enough to worry about is one long
enough to be worth a bigger chip.
