//! Traits only. No implementations live here.
//!
//! Every source of nondeterminism a Lumen node touches — time, the network,
//! persistent storage, LED output, entropy — is one of these traits. The core
//! state machines are sans-IO and never call them directly; the *shell* around
//! a core does, and picks an implementation: `esp-idf` on hardware, a simulated
//! one in `lumen-sim`, a host one on the desktop.
//!
//! That is the whole reason deterministic replay works: there is nothing else
//! to inject.

#![no_std]
#![forbid(unsafe_code)]

/// Microseconds on the **show clock** — monotonic, mesh-wide, never steps.
///
/// Rendering only ever reads this. Wall time is a separate, optional concern
/// (see [`WallClock`]) and schedules degrade explicitly without it.
pub type ShowTimeUs = u64;

/// The monotonic show clock, disciplined towards the mesh timebase.
pub trait Clock {
    /// Current show time. Monotonic: successive calls never go backwards.
    fn now_us(&self) -> ShowTimeUs;

    /// Adjust rate and offset towards the mesh timebase.
    ///
    /// Implementations slew rather than step; a stepped render clock is a
    /// visible glitch.
    fn discipline(&mut self, offset_us: i64);
}

/// Wall-clock time, when the node has any. Optional by design.
pub trait WallClock {
    /// Unix time in microseconds, or `None` when no trusted source is known.
    fn unix_us(&self) -> Option<u64>;
}

/// Datagram network access — unicast and multicast, no framing opinions.
pub trait Net {
    type Error;

    /// Send `buf` to `addr`.
    fn send_to(&mut self, addr: &SocketAddr, buf: &[u8]) -> Result<(), Self::Error>;

    /// Receive one datagram into `buf`, returning its length and sender.
    ///
    /// Non-blocking: returns `Ok(None)` when nothing is pending.
    fn recv(&mut self, buf: &mut [u8]) -> Result<Option<(usize, SocketAddr)>, Self::Error>;

    /// Join a multicast group. Channels ride multicast.
    fn join_multicast(&mut self, group: &SocketAddr) -> Result<(), Self::Error>;
}

/// An address on the local mesh. Deliberately minimal and `no_std`-friendly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SocketAddr {
    pub ip: IpAddr,
    pub port: u16,
}

/// IPv4 or IPv6, without pulling in `std::net`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IpAddr {
    V4([u8; 4]),
    V6([u8; 16]),
}

/// Persistent key/value storage — NVS on hardware, a file or a map elsewhere.
pub trait Storage {
    type Error;

    /// Read `key` into `buf`, returning its length, or `None` if absent.
    fn read(&self, key: &str, buf: &mut [u8]) -> Result<Option<usize>, Self::Error>;
    fn write(&mut self, key: &str, value: &[u8]) -> Result<(), Self::Error>;
    fn erase(&mut self, key: &str) -> Result<(), Self::Error>;
}

/// LED output. Rendering is optional: `render` is one capability among many,
/// and audio, sensor and control-surface nodes implement none of this.
pub trait LedOut {
    type Error;

    /// Number of pixels this output drives.
    fn pixel_count(&self) -> usize;

    /// Present one frame. `pixels` is post-calibration linear RGB(W).
    fn present(&mut self, pixels: &[Rgbw]) -> Result<(), Self::Error>;
}

/// One pixel after the colour pipeline. Sixteen bits per channel, so the
/// calibration matrix and temporal dithering have somewhere to go.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Rgbw {
    pub r: u16,
    pub g: u16,
    pub b: u16,
    pub w: u16,
}

/// Entropy for nonces and key material. A CSPRNG on real hardware; seeded
/// deterministically in the simulator, which is the point.
pub trait Entropy {
    fn fill(&mut self, buf: &mut [u8]);
}
