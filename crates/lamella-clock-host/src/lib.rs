//! The HOST monotonic clock: milliseconds since the SYSTEM started.
//!
//! The `no_std` interpreter core defines a clock SEAM (`Vm::set_clock`) and supplies no clock of
//! its own, because what a monotonic millisecond is differs per target: a device reads a timer
//! peripheral, and a host asks its operating system. This crate is the host half, and it is a
//! separate crate so that a device build never links it and the core never gains a `std` knob for
//! a host convenience.
//!
//!
//!
//! # What it reports, and why it is the system's clock rather than the process's
//!
//! .NET's `Environment.TickCount` is milliseconds since the SYSTEM started, so the first reading a
//! desktop program sees is the machine's uptime and is usually large. This reports that.
//!
//!
//! Differences are unaffected, which is how nearly every caller uses this: two readings subtracted
//! give the same elapsed milliseconds whatever the origin. Only an absolute reading differs.

/// Milliseconds since the system started.
///
/// Monotonic and never negative. The value wraps only at `u64`, which no plausible uptime reaches;
/// the truncation to `int` that `Environment.TickCount` performs happens in the interpreter, not
/// here, so this stays a full-width count for callers that want elapsed time.
///
#[must_use]
pub fn system_uptime_millis() -> u64 {
    platform::uptime_millis()
}

#[cfg(windows)]
mod platform {
    /// `GetTickCount64` is milliseconds since the system started, 64-bit so it does not wrap at
    /// 49.7 days the way the 32-bit `GetTickCount` does.
    pub(super) fn uptime_millis() -> u64 {
        unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    /// `/proc/uptime`'s first field is seconds since boot, as a decimal with two places.
    ///
    pub(super) fn uptime_millis() -> u64 {
        let Ok(text) = std::fs::read_to_string("/proc/uptime") else {
            return 0;
        };
        let Some(field) = text.split_whitespace().next() else {
            return 0;
        };
        let Ok(seconds) = field.parse::<f64>() else {
            return 0;
        };
        if seconds <= 0.0 { 0 } else { (seconds * 1000.0) as u64 }
    }
}

#[cfg(target_vendor = "apple")]
mod platform {
    /// `CLOCK_MONOTONIC` is Darwin's count since the system booted, and it keeps counting while the
    /// system is asleep -- the same span `GetTickCount64` and `CLOCK_BOOTTIME` report, so all three
    /// arms answer the same question rather than three neighbouring ones.
    ///
    ///
    ///
    pub(super) fn uptime_millis() -> u64 {
        let nanos = unsafe { clock_gettime_nsec_np(CLOCK_MONOTONIC) };
        nanos / 1_000_000
    }

    /// Transcribed from Darwin's `<time.h>`: `_CLOCK_MONOTONIC = 6`, available since macOS 10.12
    /// / iOS 10 / tvOS 10 / watchOS 3, all of which `target_vendor = "apple"` reaches.
    ///
    const CLOCK_MONOTONIC: u32 = 6;

    unsafe extern "C" {
        fn clock_gettime_nsec_np(clock_id: u32) -> u64;
    }
}

#[cfg(not(any(windows, target_os = "linux", target_vendor = "apple")))]
mod platform {
    /// An unrecognized host.
    ///
    pub(super) fn uptime_millis() -> u64 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::system_uptime_millis;

    /// It advances, and it is not the process's age.
    ///
    #[test]
    fn it_reports_system_uptime_and_not_process_uptime() {
        if !cfg!(any(windows, target_os = "linux", target_vendor = "apple")) {
            eprintln!("no system-uptime source on this platform; skipping");
            return;
        }
        let first = system_uptime_millis();
        const A_MINUTE: u64 = 60 * 1000;
        assert!(
            first >= A_MINUTE,
            "first reading is {first} ms -- small enough to be a process-relative origin rather than system uptime. The one innocent cause is a machine booted less than a minute ago; re-run before reading this as the defect."
        );
    }

    /// Monotonic across two readings.
    #[test]
    fn it_never_goes_backwards() {
        let first = system_uptime_millis();
        let second = system_uptime_millis();
        assert!(second >= first, "uptime went backwards: {first} then {second}");
    }

    /// Sub-second resolution -- the property a whole-seconds source cannot fake.
    ///
    #[test]
    fn it_reads_a_clock_with_sub_second_resolution() {
        if !cfg!(any(windows, target_os = "linux", target_vendor = "apple")) {
            eprintln!("no system-uptime source on this platform; skipping");
            return;
        }
        const READINGS: usize = 5;
        const SPACING: std::time::Duration = std::time::Duration::from_millis(25);
        let mut values = Vec::with_capacity(READINGS);
        for _ in 0..READINGS {
            values.push(system_uptime_millis());
            std::thread::sleep(SPACING);
        }
        assert!(
            values.iter().any(|ms| ms % 1000 != 0),
            "every reading landed exactly on a whole second: {values:?}. Over a span this short they cannot all be different multiples of 1000, so the clock is being read at WHOLE-SECOND resolution and multiplied up -- which is what a `sysctl kern.boottime` text parse does."
        );
    }

    /// Reading the clock does not spawn a process.
    ///
    #[test]
    fn reading_the_clock_does_not_spawn_a_process() {
        const CALLS: usize = 2000;
        const BUDGET: std::time::Duration = std::time::Duration::from_millis(250);
        let start = std::time::Instant::now();
        for _ in 0..CALLS {
            std::hint::black_box(system_uptime_millis());
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < BUDGET,
            "{CALLS} clock readings took {elapsed:?}, over the {BUDGET:?} budget -- that is {:?} each, which is process-spawn territory rather than a system call. Reading the clock must not shell out.",
            elapsed / u32::try_from(CALLS).expect("call count fits u32")
        );
    }
}
