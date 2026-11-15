//! The flash progress a program run shows, shared by probe and firmware transfers.

use std::time::{Duration, Instant};

/// Said only when a write is about to erase flash that needs changing.
pub const ERASING: &str = "Erasing the program area of flash...";
/// Said after a changed image has been written and verified.
pub const COMPLETE: &str = "Flash complete";

const WRITING: &str = "Writing flash: ";

/// The minimum time between chunk lines, except for the final line.
pub const CHUNK_LINE_INTERVAL: Duration = Duration::from_secs(3);

/// A reading of the monotonic clock a write's progress is paced by: an [`Instant`], or, in a build whose standard
/// library has no clock (a WebAssembly page, where `Instant::now` panics), the time since a fixed origin the host
/// keeps, as a [`Duration`].
pub trait Moment: Copy {
    /// The time from `earlier` to this reading, or none when `earlier` is the later of the two.
    fn since(self, earlier: Self) -> Duration;
}

impl Moment for Instant {
    fn since(self, earlier: Self) -> Duration {
        self.saturating_duration_since(earlier)
    }
}

impl Moment for Duration {
    fn since(self, earlier: Self) -> Duration {
        self.saturating_sub(earlier)
    }
}

/// Reports completed chunks at a bounded rate, always including the final chunk.
pub struct ChunkLines<T: Moment = Instant> {
    last_line: T,
}

impl<T: Moment> ChunkLines<T> {
    /// Start timing when writing begins, using the caller's monotonic clock.
    #[must_use]
    pub fn new(started: T) -> Self {
        Self { last_line: started }
    }

    /// Call after a chunk completes, with the same clock and the cumulative bytes written.
    /// A suppressed line does not restart the interval.
    #[must_use]
    pub fn completed(&mut self, bytes: usize, total: usize, now: T) -> Option<String> {
        if bytes != total && now.since(self.last_line) < CHUNK_LINE_INTERVAL {
            return None;
        }
        self.last_line = now;
        Some(written(bytes, total))
    }
}

/// One line for a completed chunk. Counts include only the bytes being written on this transfer.
#[must_use]
pub fn written(bytes: usize, total: usize) -> String {
    format!("{WRITING}{bytes} of {total} bytes.")
}

/// Select the run's progress from a backend's more detailed, possibly indented diagnostic lines.
#[must_use]
pub fn for_run(line: &str) -> Option<&str> {
    let line = line.trim_start();
    (line == ERASING || line == COMPLETE || line.starts_with(WRITING)).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_lines_wait_from_the_start_then_from_the_last_line() {
        let start = Instant::now();
        let mut lines = ChunkLines::new(start);
        for (milliseconds, bytes, expected) in [
            (0, 1, None),
            (2999, 2, None),
            (3000, 3, Some("Writing flash: 3 of 10 bytes.")),
            (5999, 4, None),
            (6500, 5, Some("Writing flash: 5 of 10 bytes.")),
            (9000, 6, None),
            (9499, 7, None),
            (9500, 8, Some("Writing flash: 8 of 10 bytes.")),
            (9501, 10, Some("Writing flash: 10 of 10 bytes.")),
        ] {
            assert_eq!(lines.completed(bytes, 10, start + Duration::from_millis(milliseconds)).as_deref(), expected);
        }
    }

    #[test]
    fn a_hosts_clock_since_its_origin_paces_the_lines_as_an_instant_does() {
        let start = Duration::from_millis(12_345);
        let mut lines = ChunkLines::new(start);
        for (milliseconds, bytes, expected) in [
            (2999, 1, None),
            (3000, 2, Some("Writing flash: 2 of 4 bytes.")),
            (5999, 3, None),
            (6000, 4, Some("Writing flash: 4 of 4 bytes.")),
        ] {
            assert_eq!(lines.completed(bytes, 4, start + Duration::from_millis(milliseconds)).as_deref(), expected);
        }
        let mut backwards = ChunkLines::new(Duration::from_secs(10));
        assert_eq!(backwards.completed(1, 2, Duration::from_secs(1)), None);
    }

    #[test]
    fn a_fast_write_always_says_its_final_chunk() {
        let start = Instant::now();
        assert_eq!(ChunkLines::new(start).completed(256, 256, start).as_deref(),
            Some("Writing flash: 256 of 256 bytes."));
    }

    #[test]
    fn slow_chunks_each_get_one_line() {
        let start = Instant::now();
        let mut lines = ChunkLines::new(start);
        for bytes in 1..=3 {
            assert_eq!(lines.completed(bytes, 3, start + Duration::from_secs(bytes as u64 * 4)),
                Some(format!("Writing flash: {bytes} of 3 bytes.")));
        }
    }
}
