//! Replay protection: a 64-entry sliding window per `(sender_prefix, boot_counter)`.
//!
//! The window is a bitmap of which sequence numbers have been seen relative to
//! the highest one so far. Fixed size, no allocation, constant time.

/// How many sequence numbers behind the newest one are remembered.
pub const WINDOW_BITS: u32 = 64;

/// What [`ReplayWindow::check`] decided about a datagram.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReplayVerdict {
    /// Not seen before; the window has been updated to include it.
    Fresh,
    /// Already seen. Drop it.
    Duplicate,
    /// Older than the window remembers. Drop it — the alternative is accepting a
    /// replay of something arbitrarily old.
    TooOld,
}

/// A sliding replay window for one sender within one boot.
///
/// Keyed externally by `(sender_prefix, boot_counter)`: a reboot resets the
/// sender's sequence to zero, and the boot counter is what stops that looking
/// like a flood of replays — as well as what stops nonce reuse under the same
/// key.
#[derive(Clone, Copy, Debug)]
pub struct ReplayWindow {
    highest: u32,
    /// Bit *i* set means `highest - i` has been seen. Bit 0 is `highest` itself.
    seen: u64,
    started: bool,
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplayWindow {
    pub const fn new() -> Self {
        ReplayWindow {
            highest: 0,
            seen: 0,
            started: false,
        }
    }

    /// Highest sequence number accepted so far, or `None` before the first.
    pub const fn highest(&self) -> Option<u32> {
        if self.started {
            Some(self.highest)
        } else {
            None
        }
    }

    /// Judge `sequence`, updating the window when the verdict is
    /// [`ReplayVerdict::Fresh`].
    pub fn check(&mut self, sequence: u32) -> ReplayVerdict {
        if !self.started {
            self.started = true;
            self.highest = sequence;
            self.seen = 1;
            return ReplayVerdict::Fresh;
        }

        if sequence > self.highest {
            let shift = sequence - self.highest;
            self.seen = if shift >= WINDOW_BITS {
                0
            } else {
                self.seen << shift
            };
            self.seen |= 1;
            self.highest = sequence;
            return ReplayVerdict::Fresh;
        }

        let behind = self.highest - sequence;
        if behind >= WINDOW_BITS {
            return ReplayVerdict::TooOld;
        }
        let bit = 1u64 << behind;
        if self.seen & bit != 0 {
            ReplayVerdict::Duplicate
        } else {
            self.seen |= bit;
            ReplayVerdict::Fresh
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_datagram_is_always_fresh() {
        let mut w = ReplayWindow::new();
        assert_eq!(w.highest(), None);
        assert_eq!(w.check(5000), ReplayVerdict::Fresh);
        assert_eq!(w.highest(), Some(5000));
    }

    #[test]
    fn sequential_traffic_is_all_fresh() {
        let mut w = ReplayWindow::new();
        for seq in 0..500 {
            assert_eq!(w.check(seq), ReplayVerdict::Fresh, "seq {seq}");
        }
    }

    #[test]
    fn an_exact_repeat_is_a_duplicate() {
        let mut w = ReplayWindow::new();
        w.check(10);
        assert_eq!(w.check(10), ReplayVerdict::Duplicate);
    }

    #[test]
    fn out_of_order_within_the_window_is_accepted_once() {
        // Reordering is normal on a busy AP; only a genuine repeat is a replay.
        let mut w = ReplayWindow::new();
        w.check(100);
        assert_eq!(w.check(98), ReplayVerdict::Fresh);
        assert_eq!(w.check(98), ReplayVerdict::Duplicate);
        assert_eq!(w.check(99), ReplayVerdict::Fresh);
    }

    #[test]
    fn anything_older_than_the_window_is_refused() {
        let mut w = ReplayWindow::new();
        w.check(1000);
        assert_eq!(w.check(1000 - WINDOW_BITS), ReplayVerdict::TooOld);
        assert_eq!(w.check(0), ReplayVerdict::TooOld);
        // The oldest still-remembered sequence is exactly at the edge.
        assert_eq!(w.check(1000 - WINDOW_BITS + 1), ReplayVerdict::Fresh);
    }

    #[test]
    fn a_large_jump_forward_clears_the_history() {
        // After a gap wider than the window nothing behind the new high can be
        // judged, so everything old must read as TooOld rather than Fresh.
        let mut w = ReplayWindow::new();
        w.check(1);
        w.check(1_000_000);
        assert_eq!(w.check(1), ReplayVerdict::TooOld);
        assert_eq!(w.check(1_000_000), ReplayVerdict::Duplicate);
        assert_eq!(w.check(999_999), ReplayVerdict::Fresh);
    }

    #[test]
    fn a_jump_of_exactly_the_window_width_is_handled_without_overflow() {
        // `1u64 << 64` is undefined behaviour in C and a panic in debug Rust.
        // This is the case that finds it.
        let mut w = ReplayWindow::new();
        w.check(0);
        assert_eq!(w.check(WINDOW_BITS), ReplayVerdict::Fresh);
        assert_eq!(w.check(0), ReplayVerdict::TooOld);
    }

    #[test]
    fn default_matches_new() {
        let a = ReplayWindow::default();
        let b = ReplayWindow::new();
        assert_eq!(a.highest(), b.highest());
    }
}
