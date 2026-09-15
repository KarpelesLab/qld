//! Timing and size statistics for the write stage.
//!
//! These feed `--time-trace`-style reporting and `--stats` later; for now
//! they are plain data that callers can print or aggregate.

use std::fmt;
use std::time::{Duration, Instant};

/// A phase of writing the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WritePhase {
    /// Removing the old output, creating the new file, setting its length and
    /// mapping it.
    Open,
    /// Copying section contents and applying relocations (recorded by the
    /// caller, see [`WriteStats::time`]).
    Write,
    /// Computing and patching in the build-id.
    BuildId,
    /// Writing out a buffered image, setting permissions and renaming the
    /// file into place.
    Commit,
}

impl WritePhase {
    /// Every phase, in pipeline order.
    pub const ALL: [Self; 4] = [Self::Open, Self::Write, Self::BuildId, Self::Commit];

    /// A short lowercase name, for reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Write => "write",
            Self::BuildId => "build-id",
            Self::Commit => "commit",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Open => 0,
            Self::Write => 1,
            Self::BuildId => 2,
            Self::Commit => 3,
        }
    }
}

/// How the output bytes were held while writing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Backing {
    /// A writable shared mapping of the output file.
    Mapped,
    /// Chunks rendered into heap buffers and written to the output file with
    /// positional writes (`pwrite`); nothing else of the image is held in
    /// memory.
    Written,
    /// A heap buffer written to the destination at commit, because mapping
    /// was not possible (a pipe, a device, or a file system without mmap).
    Buffered,
    /// A heap buffer returned to the caller; no file is written.
    #[default]
    Memory,
}

/// Size and per-phase elapsed time of one output write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteStats {
    /// Size of the output in bytes.
    pub bytes: u64,
    /// How the bytes were held.
    pub backing: Backing,
    /// Whether an old output file was released on a background thread.
    pub background_release: bool,
    elapsed: [Duration; 4],
}

impl WriteStats {
    /// Creates empty statistics for an output of `bytes` bytes.
    #[must_use]
    pub fn new(bytes: u64, backing: Backing) -> Self {
        Self {
            bytes,
            backing,
            ..Self::default()
        }
    }

    /// Total time recorded for `phase`.
    #[must_use]
    pub fn elapsed(&self, phase: WritePhase) -> Duration {
        self.elapsed[phase.index()]
    }

    /// Adds `duration` to `phase`.
    pub fn record(&mut self, phase: WritePhase, duration: Duration) {
        let slot = &mut self.elapsed[phase.index()];
        *slot = slot.saturating_add(duration);
    }

    /// Runs `f` and adds its elapsed time to `phase`.
    pub fn time<T>(&mut self, phase: WritePhase, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let result = f();
        self.record(phase, start.elapsed());
        result
    }

    /// Sum of all phases.
    #[must_use]
    pub fn total(&self) -> Duration {
        self.elapsed
            .iter()
            .fold(Duration::ZERO, |acc, d| acc.saturating_add(*d))
    }
}

impl fmt::Display for WriteStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes ({:?})", self.bytes, self.backing)?;
        for phase in WritePhase::ALL {
            write!(f, ", {} {:?}", phase.name(), self.elapsed(phase))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_sums() {
        let mut stats = WriteStats::new(10, Backing::Mapped);
        stats.record(WritePhase::Open, Duration::from_millis(2));
        stats.record(WritePhase::Open, Duration::from_millis(3));
        stats.record(WritePhase::Commit, Duration::from_millis(1));
        let value = stats.time(WritePhase::Write, || 7);
        assert_eq!(value, 7);
        assert_eq!(stats.elapsed(WritePhase::Open), Duration::from_millis(5));
        assert!(stats.total() >= Duration::from_millis(6));
        assert!(stats.to_string().starts_with("10 bytes (Mapped), open"));
    }
}
