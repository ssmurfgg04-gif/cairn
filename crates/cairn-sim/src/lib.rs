//! Cairn deterministic simulation (SPEC §15.1, ADR-0008): the I2 enforcement suite.
//!
//! Drives 2–4 REAL engine instances over REAL SQLite stores against the REAL in-process
//! server, under a seeded schedule: localized edits, crashes (abandon + reopen = crash
//! semantics for WAL), network partitions (injected `Unavailable`), fold + GC-shadow
//! concurrency. Assertions (a)–(d) run every schedule:
//!   (a) every acknowledged append survives every crash;
//!   (b) all live devices converge to identical state;
//!   (c) no corrupt manifest/file ever materializes;
//!   (d) GC never deletes a reachable object (shadow verify pass).
//!
//! Wall-clock jitter (AIMD backoff) affects only latency, not outcomes — logical
//! determinism. The nightly CI job runs 1,000 schedules; the default test sweep is smaller.

#![forbid(unsafe_code)]

pub mod plane;
pub mod world;

/// Assertions (a)–(d) results for one schedule.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub acked_appends_survived: bool,
    pub devices_converged: bool,
    pub no_corrupt_manifests: bool,
    pub gc_shadow_clean: bool,
    pub ticks: u64,
    pub appends_acked: u64,
}

impl Verdict {
    /// All invariants held?
    #[must_use]
    pub const fn ok(&self) -> bool {
        self.acked_appends_survived
            && self.devices_converged
            && self.no_corrupt_manifests
            && self.gc_shadow_clean
    }
}

/// Default local sweep size (nightly CI sets 1000 via CAIRN_SIM_ITERS).
#[must_use]
pub fn default_iters() -> u64 {
    std::env::var("CAIRN_SIM_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
}

/// The I2 gate: run the randomized schedule sweep; panic on any violation.
pub fn run_sweep(iters: u64, ticks: u64) {
    for seed in 1..=iters {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let verdict = rt.block_on(async { world::run_schedule(seed, ticks).await });
        assert!(
            verdict.ok(),
            "I2 VIOLATION in schedule seed {seed}: {verdict:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sim suite gate (§15.1) — assertions (a)–(d) over the sweep.
    #[test]
    fn sim_sweep_assertions_hold() {
        run_sweep(default_iters().max(2), 12);
    }
}
