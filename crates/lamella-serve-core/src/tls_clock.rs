//! The TLS engine's wall clock on a board with no battery-backed clock: a mirror of the managed
//! wall clock, kept where the engine can read it.
//!
//! The runtime tells an embedder's wall-clock sink every time the managed clock is set, whoever set
//! it: `SystemClock.Seed`, or a completed SNTP or NTS sync. [`Mirror::set`] is that sink's body and
//! [`Mirror::unix_seconds`] is the engine's time source, so the engine's date check and its
//! per-session clock policy read the one clock the program reads. Until the first set the reading
//! is 0, which the engine takes as "never set": the adaptive policy then tolerates a certificate's
//! validity dates and records that it did. From the first set on, every session checks them in
//! full.
//!
//! Between sets the reading advances with the board's monotonic clock, which the caller passes in
//! as milliseconds. It is kept in whole seconds in 32-bit atomics, because a Cortex-M has no 64-bit
//! atomics and a certificate's validity is dated in whole seconds; a reading is within a second of
//! the instant it stands for. 32-bit Unix seconds run to 2106, and 32-bit monotonic seconds to about
//! 136 years of uptime.
//!
//! A mirror outlives the evaluation that set it. A later evaluation starts with its managed clock
//! unset, but the engine keeps the time the board last learned until a program sets the clock
//! again or returns it to never set.

use core::sync::atomic::{AtomicU32, Ordering};

/// .NET ticks (100 ns since 0001-01-01) at the Unix epoch.
const UNIX_EPOCH_IN_NET_TICKS: i64 = 621_355_968_000_000_000;

/// The managed wall clock's last set, as the TLS engine reads it.
pub struct Mirror {
    /// Unix seconds at the last set, or 0 while the clock has never been set.
    anchor_unix_s: AtomicU32,
    /// The monotonic clock, in whole seconds, when that set was recorded.
    anchor_mono_s: AtomicU32,
}

impl Mirror {
    /// A mirror of a clock that has never been set.
    pub const fn new() -> Self {
        Mirror { anchor_unix_s: AtomicU32::new(0), anchor_mono_s: AtomicU32::new(0) }
    }

    /// Records what the managed clock was set to, at `now_ms` on the board's monotonic clock: the
    /// body of the board's `Vm::set_wall_clock_sink` observer.
    ///
    /// A set before 1970 is recorded as one second past the Unix epoch. That still reads as set, so
    /// the engine checks dates in full and an ancient clock fails them, rather than falling back to
    /// the never-set reading that tolerates them. A set after 2106 is recorded as the last second
    /// 32 bits hold. `None`, the clock returning to never set, is recorded as 0.
    pub fn set(&self, state: Option<i64>, now_ms: u64) {
        let Some(ticks) = state else {
            self.anchor_unix_s.store(0, Ordering::Release);
            return;
        };
        let unix_s = ticks.saturating_sub(UNIX_EPOCH_IN_NET_TICKS) / 10_000_000;
        self.anchor_mono_s.store((now_ms / 1000) as u32, Ordering::Relaxed);
        self.anchor_unix_s.store(unix_s.clamp(1, i64::from(u32::MAX)) as u32, Ordering::Release);
    }

    /// The engine's time source at `now_ms`: the recorded instant advanced by the monotonic time
    /// since it was recorded, in Unix seconds, or 0 while the managed clock has never been set.
    pub fn unix_seconds(&self, now_ms: u64) -> u64 {
        let anchor = self.anchor_unix_s.load(Ordering::Acquire);
        if anchor == 0 {
            return 0;
        }
        let anchor_mono_s = u64::from(self.anchor_mono_s.load(Ordering::Relaxed));
        u64::from(anchor).saturating_add((now_ms / 1000).saturating_sub(anchor_mono_s))
    }
}

impl Default for Mirror {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// .NET ticks at `unix_s` seconds past the Unix epoch.
    fn at_unix(unix_s: i64) -> i64 {
        UNIX_EPOCH_IN_NET_TICKS + unix_s * 10_000_000
    }

    const NOON: i64 = 1_790_596_800;

    #[test]
    fn a_clock_never_set_reads_as_never_set_at_any_uptime() {
        let mirror = Mirror::new();
        assert_eq!(mirror.unix_seconds(0), 0);
        assert_eq!(mirror.unix_seconds(86_400_000), 0);
    }

    #[test]
    fn a_set_reads_back_and_advances_with_the_monotonic_clock() {
        let mirror = Mirror::new();
        mirror.set(Some(at_unix(NOON)), 7_250);
        assert_eq!(mirror.unix_seconds(7_250), NOON as u64);
        assert_eq!(mirror.unix_seconds(10_250), NOON as u64 + 3);
        assert_eq!(mirror.unix_seconds(7_250 + 3_600_000), NOON as u64 + 3_600);
    }

    #[test]
    fn a_later_set_moves_the_anchor_rather_than_adding_to_it() {
        let mirror = Mirror::new();
        mirror.set(Some(at_unix(1_000)), 0);
        mirror.set(Some(at_unix(NOON)), 60_000);
        assert_eq!(mirror.unix_seconds(60_000), NOON as u64);
        assert_eq!(mirror.unix_seconds(70_000), NOON as u64 + 10);
    }

    #[test]
    fn a_set_at_or_before_1970_still_reads_as_set() {
        let mirror = Mirror::new();
        mirror.set(Some(at_unix(0)), 0);
        assert_eq!(mirror.unix_seconds(0), 1, "the epoch itself is not the never-set reading");
        mirror.set(Some(0), 0);
        assert_eq!(mirror.unix_seconds(0), 1, "0001-01-01 is set, and fails every date check");
        mirror.set(Some(i64::MIN), 0);
        assert_eq!(mirror.unix_seconds(0), 1, "no tick value overflows the conversion");
    }

    #[test]
    fn a_set_past_2106_holds_at_the_last_second_32_bits_carry() {
        let mirror = Mirror::new();
        mirror.set(Some(i64::MAX), 0);
        assert_eq!(mirror.unix_seconds(0), u64::from(u32::MAX));
        assert_eq!(mirror.unix_seconds(5_000), u64::from(u32::MAX) + 5, "and still advances");
    }

    #[test]
    fn returning_to_never_set_reads_zero_until_the_next_set() {
        let mirror = Mirror::new();
        mirror.set(Some(at_unix(NOON)), 1_000);
        mirror.set(None, 2_000);
        assert_eq!(mirror.unix_seconds(3_000), 0);
        mirror.set(Some(at_unix(NOON)), 4_000);
        assert_eq!(mirror.unix_seconds(4_000), NOON as u64);
    }
}
