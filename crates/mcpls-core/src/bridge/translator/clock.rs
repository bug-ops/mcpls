//! Injectable time source for the respawn-backoff logic in [`super::respawn`].
//!
//! Production always uses [`SystemClock`]; tests use [`FakeClock`] to
//! advance time deterministically instead of sleeping in real time.

use std::time::Instant;

/// A source of the current instant, abstracting over [`Instant::now`] so
/// respawn-backoff tests can advance time deterministically instead of
/// sleeping in real time.
pub(super) trait Clock: std::fmt::Debug + Send + Sync {
    /// The current instant, per this clock's notion of time.
    fn now(&self) -> Instant;
}

/// Production [`Clock`]: delegates directly to [`Instant::now`].
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Test-only [`Clock`] with a settable, advanceable [`Instant`], so
/// respawn-backoff tests can assert on elapsed-time behavior without
/// sleeping in real time.
#[cfg(test)]
#[derive(Debug)]
pub(super) struct FakeClock {
    now: std::sync::Mutex<Instant>,
}

#[cfg(test)]
impl FakeClock {
    /// A `FakeClock` initialized to the current real instant.
    pub(super) fn new() -> Self {
        Self {
            now: std::sync::Mutex::new(Instant::now()),
        }
    }

    /// Advance this clock's reported time by `duration`.
    pub(super) fn advance(&self, duration: std::time::Duration) {
        let mut now = crate::bridge::lock_std(&self.now);
        *now += duration;
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *crate::bridge::lock_std(&self.now)
    }
}
