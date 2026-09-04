/* The Lumen core, for firmware in C.
 *
 * The wire codec and the bytecode VM, compiled to a static library. Your
 * firmware keeps WiFi, sockets and the LED output; this decides what a datagram
 * says and what colour each pixel is.
 *
 * Rendering here is bit-identical with every other device in the mesh. That is
 * the reason to link this rather than reimplement it: the VM is fixed point
 * precisely so a gradient spanning six strips looks like one gradient, and a
 * second implementation in C is where that quietly stops being true.
 *
 * Nothing here allocates. You provide the machine's storage, you hold the
 * bytecode, you own the pixel buffer. Every function checks its arguments and
 * returns an error rather than reading past the end of anything.
 *
 * Licence: Apache-2.0.
 */

#ifndef LUMEN_H
#define LUMEN_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- results ---------------------------------------------------------- */

#define LUMEN_OK            0
#define LUMEN_NULL         (-1)   /* a pointer argument was null            */
#define LUMEN_TOO_SMALL    (-2)   /* a buffer was too small, or a range bad */
#define LUMEN_BAD_PROGRAM  (-3)   /* not a program this VM can run          */
#define LUMEN_FAULTED      (-4)   /* the program faulted while running      */
#define LUMEN_BAD_DATAGRAM (-5)   /* not a datagram of this protocol        */

/* Bytes per LED in a rendered buffer: R, G, B. */
#define LUMEN_BYTES_PER_PIXEL 3

/* ---- the machine ------------------------------------------------------ */

/* Opaque. Its size and alignment come from the functions below rather than
 * from this header, so a firmware built against one version keeps working
 * when the VM's registers change. */
typedef struct LumenMachine LumenMachine;

size_t lumen_machine_size(void);
size_t lumen_machine_align(void);

/* Build a machine in storage you own.
 *
 *   static uint64_t backing[64];               // aligned by being uint64_t
 *   LumenMachine *m;
 *   if (lumen_machine_init(backing, sizeof backing, &m) != LUMEN_OK) { ... }
 *
 * The storage must outlive every use of the machine. */
int32_t lumen_machine_init(void *storage, size_t len, LumenMachine **out);

/* Copy a machine, for a second core rendering the other half of a strip.
 * See the note on lumen_render_range. */
int32_t lumen_machine_clone(const LumenMachine *from, LumenMachine *to);

/* Cap the fuel one section invocation may spend. A backstop against a program
 * that costs more than it promised, not the primary check. */
int32_t lumen_machine_set_budget(LumenMachine *machine, uint32_t units);

/* ---- programs --------------------------------------------------------- */

/* Check that bytes is a program, and report what it costs per pixel in budget
 * units, where one unit is 100 ns on an ESP32-C3. Pass NULL for budget_out if
 * you do not want it.
 *
 * Call this once when a program arrives, not once a frame: the answer cannot
 * change, and re-checking every frame costs a measurable part of one. */
int32_t lumen_program_check(const uint8_t *bytes, size_t len, uint32_t *budget_out);

/* ---- rendering -------------------------------------------------------- */

/* Run the frame section for a moment in the show.
 *
 * Call once per frame, before the pixels. The VM's whole performance story is
 * that work which does not vary per pixel happens here instead of per LED.
 *
 * t_q16 is show time in seconds as Q16.16. Use lumen_time_q16 to get there
 * from microseconds. */
int32_t lumen_frame(LumenMachine *machine, const uint8_t *bytes, size_t len,
                    int32_t t_q16, int32_t dt_q16);

/* Render the whole strip into linear_out, which must hold count * 3 int32_t.
 *
 * The output is **linear light** as Q16.16, not codes: rendering produces light
 * and lumen_encode turns light into what a strip consumes. They are separate
 * because deciding a frame is over its power budget needs the whole frame before
 * any of it can be scaled, and doing that inside here would mean either
 * allocating a staging buffer - which nothing in this library does - or derating
 * from the previous frame, which would make this firmware disagree with a Rust
 * one about the same show.
 *
 * One call for the strip. There is deliberately no per-pixel entry point: it
 * would invite a loop across the language boundary, and the same shape measured
 * on this project's Android binding cost over two hundred times what the
 * batched call did. */
int32_t lumen_render(LumenMachine *machine, const uint8_t *bytes, size_t len,
                     uint16_t count, int32_t *linear_out, size_t out_len);

/* Render LEDs [from, to) of a strip of `count`, into (to - from) * 3 bytes.
 *
 * For a dual-core device splitting the work. The pixels of a frame are
 * independent, so two cores rendering halves produce exactly the bytes one core
 * rendering all of them would - checked by a test, because a device that
 * rendered differently on two cores would disagree with the rest of the mesh.
 *
 * Each core needs its OWN machine. Run lumen_frame once on one of them, then
 * lumen_machine_clone into the other: the hoisted results live in the machine's
 * registers and the second core needs them.
 *
 * `count` is the whole strip, not the slice. Passing the slice length instead
 * makes each half render as though it were the whole strip, which looks like a
 * mirrored effect rather than an error. */
int32_t lumen_render_range(LumenMachine *machine, const uint8_t *bytes, size_t len,
                           uint16_t from, uint16_t to, uint16_t count,
                           int32_t *linear_out, size_t out_len);

/* ---- the output stage -------------------------------------------------- */

/* How this device turns a frame into codes.
 *
 * A zeroed struct works: full brightness, no supply limit, no dithering. So a
 * firmware that does not care can memset one and pass it. */
typedef struct {
    /* Global brightness as Q16.16. 0 means full, not black - the struct is
     * designed to be memset to zero and still work, and a device that wants
     * darkness should stop rendering rather than render black sixty times a
     * second. */
    int32_t brightness_q16;

    /* What the supply can give, in milliamps. 0 disables derating.
     *
     * Worth setting. Thirty SK6812 at full white want about 1.2 A, and a board
     * that browns out mid-frame looks exactly like a driver that cannot hold
     * one. An over-budget frame is dimmed uniformly rather than clipped, so it
     * keeps its colours. */
    uint32_t budget_ma;

    /* Dither state: count * 3 int32_t, zeroed at startup and kept between
     * frames. NULL turns dithering off.
     *
     * Worth providing. Eight bits of linear PWM cannot represent anything
     * below 1/255, so without this the dark end of every fade arrives in a few
     * visible steps and then stops early - which reads as an effect that is
     * wrong rather than a strip that is coarse. The dither is deterministic, so
     * two devices showing one gradient stay in step. */
    int32_t *residual;
} LumenOutput;

/* Turn a rendered frame into the bytes a strip consumes.
 *
 * `linear` is what lumen_render wrote. `rgb_out` takes three bytes per LED.
 * `output` may be NULL, meaning defaults.
 *
 * draw_ua_out receives the predicted draw in microamps and derated_q16_out the
 * factor the frame was scaled by; either may be NULL. Worth reading: a strip
 * quietly at 40% because its supply is too small looks exactly like an effect
 * that is quietly wrong, and the two are found in completely different places.
 *
 * There is no gamma here, deliberately. A WS2812-class LED's PWM is
 * proportional to the light it emits and a Lumen colour is already linear light,
 * so an sRGB curve on the way out would make every strip brighter than the
 * effect asked for. The problem it is usually reached for is quantisation, and
 * that is what the dithering above is for. */
int32_t lumen_encode(const int32_t *linear, uint16_t count,
                     uint8_t *rgb_out, size_t out_len,
                     const LumenOutput *output,
                     uint32_t *draw_ua_out, int32_t *derated_q16_out);

/* ---- the wire --------------------------------------------------------- */

/* Read a datagram's header, without decrypting or decoding the payload.
 *
 * This is what a receiver decides from: whether the datagram belongs to this
 * mesh, and whether it is already too late to matter. Both answerable from the
 * header alone, which is why the header is not encrypted. Any out pointer may
 * be NULL. */
int32_t lumen_header_read(const uint8_t *bytes, size_t len,
                          uint8_t *msg_type_out, uint16_t *mesh_prefix_out,
                          uint64_t *show_time_out);

/* Show time in microseconds to the Q16.16 seconds the VM reads.
 *
 * Here rather than in your code because the split is easy to get wrong: Q16
 * saturates around 32768, so a one-step conversion loses the fraction entirely
 * once a show has been running for a while. This wraps instead, which is
 * invisible to an effect reading t through fract or a wave. */
int32_t lumen_time_q16(uint64_t micros);

/* ---- what you must provide -------------------------------------------- */

/* Called if the library panics. It must not return.
 *
 * Do something visible. A device that goes dark with no explanation is the
 * outcome this project's rules single out as worst:
 *
 *   void lumen_panic(void) { os_printf("lumen: panic\n"); system_restart(); }
 */
extern void lumen_panic(void) __attribute__((noreturn));

#ifdef __cplusplus
}
#endif

#endif /* LUMEN_H */
