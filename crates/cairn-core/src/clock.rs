//! Clock abstraction (SPEC I4: server clock only; client ts informational).
//!
//! Production uses `WallClock`; the deterministic sim (ADR-0008) injects virtual time so lease
//! expiry, compaction and backoff are fully logical.

use std::time::{SystemTime, UNIX_EPOCH};

/// Time source. Never call this for ordering decisions on the server — journal seq is the only
/// ordering primitive (I4). Clocks are for TTLs, retries, and instrumentation.
pub trait SystemClock: Send + Sync {
    /// UTC milliseconds since epoch.
    fn now_millis(&self) -> i64;
}

/// Wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct WallClock;

impl SystemClock for WallClock {
    fn now_millis(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }
}

/// Fixed virtual clock for tests/sim.
#[derive(Debug)]
pub struct FixedClock(pub std::sync::atomic::AtomicI64);

impl FixedClock {
    /// New fixed clock at `millis`.
    #[must_use]
    pub fn new(millis: i64) -> Self {
        FixedClock(std::sync::atomic::AtomicI64::new(millis))
    }

    /// Advance by `ms`.
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl SystemClock for FixedClock {
    fn now_millis(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_clock_is_plausible() {
        assert!(WallClock.now_millis() > 1_700_000_000_000); // after Nov 2023
    }

    #[test]
    fn fixed_clock_advances() {
        let c = FixedClock::new(1_000);
        c.advance(500);
        assert_eq!(c.now_millis(), 1_500);
    }
}
