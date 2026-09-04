//! The Lumen core, as a static library for ESP8266 firmware.
//!
//! Everything of substance is in `lumen-capi`. This adds the one thing a
//! freestanding library needs and a testable crate cannot have: somewhere for a
//! panic to go.
//!
//! See `nodes/esp8266/README.md` for how a firmware links it.

#![no_std]

// `lumen-capi` exports its symbols with `#[no_mangle]`, and nothing here calls
// them — so without this the linker is entitled to drop the whole crate and
// hand the firmware an archive with no Lumen symbols in it. Re-exporting keeps
// them reachable.
pub use lumen_capi::*;

/// Where a panic goes.
///
/// Calls out to the firmware, which knows what it wants: a log line over the
/// serial the installer is watching, a watchdog reset, or lighting the strip a
/// diagnostic colour. Looping silently would leave a device dark with no way to
/// find out why, and "a device is never dark because of software" is one of the
/// four rules this project checks every change against.
///
/// A firmware that has not thought about it can supply the three-line version:
///
/// ```c
/// void lumen_panic(void) { os_printf("lumen: panic\n"); system_restart(); }
/// ```
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    extern "C" {
        fn lumen_panic() -> !;
    }
    unsafe { lumen_panic() }
}
