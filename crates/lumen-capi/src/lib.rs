//! The C ABI over the wire codec and the bytecode VM.
//!
//! For a device whose radio has no Rust driver. The firmware owns WiFi, the
//! sockets and the LED output in whatever language its SDK speaks, and links
//! this for the parts that must agree with the rest of the mesh: how a datagram
//! is read, and what a program computes for a pixel.
//!
//! The ESP8266 is why this exists. Its radio is a closed blob from the NONOS
//! SDK, there is no Rust binding for it and writing one is months of work — but
//! `rustc` targets `xtensa-esp8266-none-elf` directly, so the *portable* half
//! compiles to a static library the existing C firmware can link. Rendering
//! then stays bit-identical with every other device, which is the whole reason
//! the VM is fixed point and precisely what a second implementation in C would
//! lose.
//!
//! # Nothing here allocates
//!
//! The caller owns every byte. A machine is built in storage the caller
//! provides, a program borrows the bytecode the caller is holding, and pixels
//! are written into the caller's buffer. That is not an embedded affectation —
//! it is what lets this link into a firmware whose allocator is already spoken
//! for by a WiFi stack.
//!
//! # Every function is safe to call with rubbish
//!
//! A null pointer, a short buffer, a length that lies: each returns an error
//! code rather than reading past the end. A C caller has no borrow checker and
//! this is the boundary where that stops being true, so the checks are here
//! rather than in a comment asking for care.

#![no_std]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;

use lumen_proto::header::Header;
use lumen_vm::output::{Output, PowerModel};
use lumen_vm::program::Program;
use lumen_vm::q16::Q16;
use lumen_vm::vm::{Machine, NoUniforms, PixelInputs, PixelOutput};

/// What a call returned. Zero is success; everything else is negative, so a
/// caller can test `< 0` without knowing the list.
pub const LUMEN_OK: i32 = 0;
/// A pointer argument was null.
pub const LUMEN_NULL: i32 = -1;
/// A buffer was too small for what was asked of it.
pub const LUMEN_TOO_SMALL: i32 = -2;
/// The bytes are not a program this VM can run.
pub const LUMEN_BAD_PROGRAM: i32 = -3;
/// The program faulted while running.
pub const LUMEN_FAULTED: i32 = -4;
/// The bytes are not a datagram of this protocol.
pub const LUMEN_BAD_DATAGRAM: i32 = -5;

/// One LED's colour, as the firmware will clock it out.
///
/// Eight bits per channel because that is what a WS2812 takes. The VM works in
/// Q16 and the conversion happens here, once, so every implementation rounds the
/// same way — a device that rounded differently would be visibly out of step on
/// a gradient shared across a room.
pub const BYTES_PER_PIXEL: usize = 3;

/// A VM, in storage the caller owns.
///
/// Opaque on purpose: its size is [`lumen_machine_size`] and its alignment
/// [`lumen_machine_align`], and a caller that hard-codes either will break on
/// the next release rather than at the next VM change.
#[repr(C)]
pub struct LumenMachine {
    _private: [u8; 0],
}

/// Bytes of storage a machine needs.
#[no_mangle]
pub extern "C" fn lumen_machine_size() -> usize {
    core::mem::size_of::<Machine>()
}

/// Alignment that storage needs.
#[no_mangle]
pub extern "C" fn lumen_machine_align() -> usize {
    core::mem::align_of::<Machine>()
}

/// Build a machine in `storage`.
///
/// `storage` must be at least [`lumen_machine_size`] bytes and aligned to
/// [`lumen_machine_align`]. The returned pointer is `storage`; it is returned
/// rather than being implicit so a caller can check for failure in one place.
///
/// # Safety
///
/// `storage` must point to at least `len` writable bytes that outlive every use
/// of the returned machine.
#[no_mangle]
pub unsafe extern "C" fn lumen_machine_init(
    storage: *mut c_void,
    len: usize,
    out: *mut *mut LumenMachine,
) -> i32 {
    if storage.is_null() || out.is_null() {
        return LUMEN_NULL;
    }
    if len < lumen_machine_size() {
        return LUMEN_TOO_SMALL;
    }
    if !(storage as usize).is_multiple_of(lumen_machine_align()) {
        // Misaligned storage would be undefined behaviour to write through, and
        // a C caller has nothing that would have caught it.
        return LUMEN_TOO_SMALL;
    }
    let m = storage as *mut Machine;
    m.write(Machine::new());
    *out = storage as *mut LumenMachine;
    LUMEN_OK
}

/// Cap the fuel one section invocation may spend.
///
/// A backstop rather than the primary check: the compiler already promised a
/// budget and a device already decided it fits. This is what stops a program
/// that lied from taking the frame with it.
///
/// # Safety
///
/// `machine` must come from [`lumen_machine_init`].
#[no_mangle]
pub unsafe extern "C" fn lumen_machine_set_budget(machine: *mut LumenMachine, units: u32) -> i32 {
    let Some(m) = (machine as *mut Machine).as_mut() else {
        return LUMEN_NULL;
    };
    m.set_budget(units);
    LUMEN_OK
}

/// Check that `bytes` is a program, and report what it will cost per pixel.
///
/// Call once when a program arrives, not once per frame: the answer cannot
/// change, and a device that re-validated every frame would spend a measurable
/// part of it doing so.
///
/// # Safety
///
/// `bytes` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn lumen_program_check(
    bytes: *const u8,
    len: usize,
    budget_out: *mut u32,
) -> i32 {
    let Some(slice) = as_slice(bytes, len) else {
        return LUMEN_NULL;
    };
    match Program::parse(slice) {
        Ok(p) => {
            if !budget_out.is_null() {
                *budget_out = p.budget;
            }
            LUMEN_OK
        }
        Err(_) => LUMEN_BAD_PROGRAM,
    }
}

/// Run the `frame` section for a moment in the show.
///
/// `t_q16` is show time in seconds as Q16.16 — the same fixed point the VM uses
/// throughout, so a firmware holding microseconds converts once here rather than
/// letting a float in through the back door.
///
/// `dt_q16` is the time since the previous frame, and it is not optional. An
/// effect writes a trail as `pow(decay, dt * 60)` so that it looks the same on a
/// 30 fps device as on a 60 fps one; pass zero and every trail becomes
/// permanent, which on a real strip looks like pixels sticking on and staying
/// there. Pass what your frame timer actually measured, not what it aimed for.
///
/// Must be called before the pixels of that frame. The whole performance story
/// of this VM is that hoisted work happens once here instead of once per LED.
///
/// # Safety
///
/// `machine` must come from [`lumen_machine_init`]; `bytes` must point to `len`
/// readable bytes.
#[no_mangle]
pub unsafe extern "C" fn lumen_frame(
    machine: *mut LumenMachine,
    bytes: *const u8,
    len: usize,
    t_q16: i32,
    dt_q16: i32,
) -> i32 {
    let Some(m) = (machine as *mut Machine).as_mut() else {
        return LUMEN_NULL;
    };
    let Some(slice) = as_slice(bytes, len) else {
        return LUMEN_NULL;
    };
    let Ok(program) = Program::parse(slice) else {
        return LUMEN_BAD_PROGRAM;
    };
    match m.run_frame_at(&program, Q16(t_q16), Q16(dt_q16), &mut NoUniforms) {
        Ok(()) => LUMEN_OK,
        Err(_) => LUMEN_FAULTED,
    }
}

/// Render a whole strip into `rgb_out`, which must hold `count * 3` bytes.
///
/// **One call for the strip, not one per pixel.** A per-pixel entry point is
/// deliberately absent: it invites a loop across the language boundary, and the
/// same shape measured on the Android binding cost more than two hundred times
/// what the batched call did. The boundary here is cheaper than that one, and it
/// is still the wrong place to put a loop.
///
/// A pixel the program declines to light is written black rather than skipped,
/// because the caller indexes by LED and a short buffer would shift every colour
/// after the first masked pixel.
///
/// # Safety
///
/// `machine` must come from [`lumen_machine_init`]; `bytes` must point to `len`
/// readable bytes; `rgb_out` must point to `out_len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn lumen_render(
    machine: *mut LumenMachine,
    bytes: *const u8,
    len: usize,
    count: u16,
    linear_out: *mut i32,
    out_len: usize,
) -> i32 {
    lumen_render_range(machine, bytes, len, 0, count, count, linear_out, out_len)
}

/// Render part of a strip, for a firmware splitting the work.
///
/// `from` and `to` are LED indices; `count` is the whole strip, because `u` and
/// `index` are relative to the strip rather than to the slice. Getting that
/// wrong would make each half render as if it were the whole, which looks like a
/// mirrored effect rather than a crash.
///
/// This is what a dual-core device uses: the pixels of a frame are independent,
/// so two cores rendering halves produce exactly what one core rendering all of
/// them would. Each core needs its **own machine**, initialised from the same
/// frame section — see `lumen_machine_clone`.
///
/// # Safety
///
/// As [`lumen_render`], and `rgb_out` must hold `(to - from) * 3` bytes.
#[no_mangle]
pub unsafe extern "C" fn lumen_render_range(
    machine: *mut LumenMachine,
    bytes: *const u8,
    len: usize,
    from: u16,
    to: u16,
    count: u16,
    linear_out: *mut i32,
    out_len: usize,
) -> i32 {
    let Some(m) = (machine as *mut Machine).as_mut() else {
        return LUMEN_NULL;
    };
    let Some(slice) = as_slice(bytes, len) else {
        return LUMEN_NULL;
    };
    if linear_out.is_null() {
        return LUMEN_NULL;
    }
    if from > to || to > count {
        return LUMEN_TOO_SMALL;
    }
    let needed = (to - from) as usize * BYTES_PER_PIXEL;
    if out_len < needed {
        return LUMEN_TOO_SMALL;
    }
    let Ok(program) = Program::parse(slice) else {
        return LUMEN_BAD_PROGRAM;
    };
    let out = core::slice::from_raw_parts_mut(linear_out, needed);

    for i in from..to {
        let inputs = inputs_for(i, count);
        let pixel = match m.run_pixel(&program, &inputs, &mut NoUniforms) {
            Ok(p) => p,
            Err(_) => return LUMEN_FAULTED,
        };
        let (r, g, b) = channels(pixel);
        let at = (i - from) as usize * BYTES_PER_PIXEL;
        out[at] = r.0;
        out[at + 1] = g.0;
        out[at + 2] = b.0;
    }
    LUMEN_OK
}

/// Turn a rendered frame into the codes a strip consumes.
///
/// `linear` is what [`lumen_render`] wrote: three Q16.16 values per LED, in
/// linear light. `rgb_out` takes three bytes per LED.
///
/// Separate from rendering because the two are separate jobs, and because
/// deciding a frame is over its power budget needs the whole frame before any of
/// it can be scaled. Doing that inside `lumen_render` would mean either
/// allocating a staging buffer — which nothing in this ABI does — or derating
/// from the previous frame, which would make a C firmware and a Rust firmware
/// disagree about the same show.
///
/// Returns the predicted draw in microamps through `draw_ua_out`, and the factor
/// the frame was scaled by through `derated_q16_out`; either may be null. Worth
/// reading: a strip quietly at 40% because its supply is too small looks exactly
/// like an effect that is quietly wrong, and the two are found in completely
/// different places.
///
/// # Safety
///
/// `linear` must point at `count * 3` readable `int32_t`, `rgb_out` at
/// `out_len` writable bytes. `output` may be null, meaning defaults.
#[no_mangle]
pub unsafe extern "C" fn lumen_encode(
    linear: *const i32,
    count: u16,
    rgb_out: *mut u8,
    out_len: usize,
    output: *const LumenOutput,
    draw_ua_out: *mut u32,
    derated_q16_out: *mut i32,
) -> i32 {
    if linear.is_null() || rgb_out.is_null() {
        return LUMEN_NULL;
    }
    let channels = count as usize * BYTES_PER_PIXEL;
    if out_len < channels {
        return LUMEN_TOO_SMALL;
    }

    // `Q16` is a transparent wrapper over `i32`, so the caller's buffer is
    // already the right shape and no copy is needed.
    let linear = core::slice::from_raw_parts(linear as *const Q16, channels);
    let out = core::slice::from_raw_parts_mut(rgb_out, channels);

    let (stage, residual) = match output.as_ref() {
        Some(cfg) => cfg.stage(channels),
        None => (Output::new(), None),
    };
    let report = stage.encode(linear, residual, out);

    if let Some(p) = draw_ua_out.as_mut() {
        *p = report.draw_ua;
    }
    if let Some(p) = derated_q16_out.as_mut() {
        *p = report.derated_to.0;
    }
    LUMEN_OK
}

/// Copy a machine, so a second core can render from the same frame state.
///
/// The `frame` section is run once, on one core, and its results live in the
/// machine's registers. A second core rendering the other half of the strip
/// needs those results and must not share the registers it will then write, so
/// it gets a copy.
///
/// # Safety
///
/// Both pointers must come from [`lumen_machine_init`].
#[no_mangle]
pub unsafe extern "C" fn lumen_machine_clone(
    from: *const LumenMachine,
    to: *mut LumenMachine,
) -> i32 {
    let Some(src) = (from as *const Machine).as_ref() else {
        return LUMEN_NULL;
    };
    let Some(dst) = (to as *mut Machine).as_mut() else {
        return LUMEN_NULL;
    };
    *dst = src.clone();
    LUMEN_OK
}

/// Read a datagram's header without decrypting or decoding its payload.
///
/// What a receiver decides from: whether it belongs to this mesh, and whether it
/// is already too late to matter. Both answerable from the header alone, which
/// is why the header is not encrypted.
///
/// # Safety
///
/// `bytes` must point to `len` readable bytes; the out pointers, if not null,
/// must be writable.
#[no_mangle]
pub unsafe extern "C" fn lumen_header_read(
    bytes: *const u8,
    len: usize,
    msg_type_out: *mut u8,
    mesh_prefix_out: *mut u16,
    show_time_out: *mut u64,
) -> i32 {
    let Some(slice) = as_slice(bytes, len) else {
        return LUMEN_NULL;
    };
    let Ok(header) = Header::decode(slice) else {
        return LUMEN_BAD_DATAGRAM;
    };
    if !msg_type_out.is_null() {
        *msg_type_out = header.msg_type;
    }
    if !mesh_prefix_out.is_null() {
        // Two bytes on the wire, handed over as one number so a C caller can
        // compare it without caring about byte order.
        *mesh_prefix_out = u16::from_le_bytes(header.mesh_prefix);
    }
    if !show_time_out.is_null() {
        *show_time_out = header.show_time_us;
    }
    LUMEN_OK
}

/// Show time in microseconds to the Q16.16 seconds the VM takes.
///
/// Here rather than in the caller because the split between whole seconds and
/// the fraction is easy to get wrong: Q16 saturates around 32 768, so a show
/// running for hours loses the fraction entirely if the conversion is done in
/// one step.
#[no_mangle]
pub extern "C" fn lumen_time_q16(micros: u64) -> i32 {
    // One implementation, in the VM, shared with every device that renders in
    // Rust. It used to be written out here as well, and the two copies drifted:
    // this one was right and the other overflowed its sub-second part.
    Q16::from_micros(micros).0
}

// ---- internals -------------------------------------------------------------

unsafe fn as_slice<'a>(bytes: *const u8, len: usize) -> Option<&'a [u8]> {
    if bytes.is_null() {
        return None;
    }
    Some(core::slice::from_raw_parts(bytes, len))
}

/// Inputs for one LED of a strip, as a linear projection produces them.
fn inputs_for(index: u16, count: u16) -> PixelInputs {
    let u = Q16::from_ratio(index as i32, count.max(1) as i32);
    PixelInputs {
        x: u,
        y: Q16::ZERO,
        z: Q16::ZERO,
        lx: u,
        ly: Q16::ZERO,
        lz: Q16::ZERO,
        index: Q16::from_int(index.min(i16::MAX as u16) as i16),
        count: Q16::from_int(count.min(i16::MAX as u16) as i16),
        u,
        uv_x: u,
        uv_y: Q16::HALF,
        prev: [Q16::ZERO; 3],
    }
}

fn channels(pixel: PixelOutput) -> (Q16, Q16, Q16) {
    match pixel {
        PixelOutput::Rgb { r, g, b } => (r, g, b),
        PixelOutput::Rgbw { r, g, b, .. } => (r, g, b),
        // A pixel the program declined to light, and a colour temperature this
        // entry point has no white channel to express. Black either way, so the
        // caller's buffer stays indexed by LED.
        _ => (Q16::ZERO, Q16::ZERO, Q16::ZERO),
    }
}

/// How a firmware wants a frame turned into codes.
///
/// A zeroed struct is a working default — full brightness, no supply limit, no
/// dithering — so a firmware that does not care can `memset` one and pass it.
/// Brightness treats `0` as "unset" for exactly that reason: a device that wants
/// black should stop rendering rather than render black sixty times a second.
#[repr(C)]
pub struct LumenOutput {
    /// Global brightness as Q16.16. `0` means full.
    pub brightness_q16: i32,
    /// What the supply can give, in milliamps. `0` disables derating.
    ///
    /// Worth setting. Thirty SK6812 at full white want about 1.2 A, and a board
    /// that browns out mid-frame looks exactly like a driver that cannot hold
    /// one.
    pub budget_ma: u32,
    /// Dither state: `count * 3` `int32_t`, zeroed at startup and carried
    /// between frames. Null turns dithering off.
    ///
    /// Worth providing. Without it every part of a value smaller than one code
    /// in 255 is lost, so the dark end of a fade arrives in a few visible steps
    /// and then stops early — which reads as an effect that is wrong rather than
    /// a strip that is coarse.
    pub residual: *mut i32,
}

impl LumenOutput {
    /// The lifetime is the caller's, not this struct's: the residual is a
    /// pointer the firmware owns and keeps between frames, and tying it to
    /// `&self` would claim the config outlives it, which is backwards.
    ///
    /// # Safety
    ///
    /// `channels` must be the number of `int32_t` behind `self.residual`, and
    /// they must live at least as long as `'a`.
    unsafe fn stage<'a>(&self, channels: usize) -> (Output, Option<&'a mut [i32]>) {
        let mut output = Output::new();
        if self.brightness_q16 > 0 {
            output.brightness = Q16(self.brightness_q16.min(Q16::ONE.0));
        }
        if self.budget_ma > 0 {
            output.power = Some(PowerModel::ws2812(self.budget_ma));
        }
        let residual = if self.residual.is_null() {
            None
        } else {
            Some(core::slice::from_raw_parts_mut(self.residual, channels))
        };
        (output, residual)
    }
}

#[cfg(test)]
mod tests;
