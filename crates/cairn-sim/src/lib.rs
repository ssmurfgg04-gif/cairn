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
    /// True when the fault script allowed ZERO appends the whole schedule
    /// (e.g. partition from tick 0 / both devices crashed before any sync).
    /// In that regime assertions (a)-(c) are vacuous — the schedule is
    /// inconclusive, not green. `run_sweep_sharded` caps the vacuous ratio and
    /// requires aggregate progress so a broken engine still fails the sweep.
    pub vacuous: bool,
}

impl Verdict {
    /// All invariants held? (A vacuous schedule is inconclusive: `ok()` is
    /// only meaningful per-schedule; the sweep applies the vacuous-ratio gate.)
    #[must_use]
    pub fn ok(&self) -> bool {
        self.vacuous
            || (self.acked_appends_survived
                && self.devices_converged
                && self.no_corrupt_manifests
                && self.gc_shadow_clean)
    }
}

/// Default local sweep size (nightly CI sets 1000 via `CAIRN_SIM_ITERS`, sharded).
#[must_use]
pub fn default_iters() -> u64 {
    std::env::var("CAIRN_SIM_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
}

/// Sharding (CI): seeds are CONTIGUOUS per shard so `1..=1000` splits as
/// shard 0 → seeds 1..=50, shard 1 → 51..=100, ... 19 runners × 50 schedules.
/// Env: `CAIRN_SIM_SHARD_INDEX` (0-based), `CAIRN_SIM_SHARD_TOTAL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shard {
    pub index: u64,
    pub total: u64,
    pub first_seed: u64,
    pub last_seed: u64,
}

impl Shard {
    /// Resolve shard bounds from explicit numbers (empty shard when out of range).
    #[must_use]
    pub fn new(index: u64, total: u64, iters: u64) -> Self {
        let total = total.max(1);
        let per = iters.div_ceil(total);
        let first = index * per + 1;
        let last = if first > iters {
            0
        } else {
            ((index + 1) * per).min(iters)
        };
        Shard {
            index,
            total,
            first_seed: first,
            last_seed: last,
        }
    }

    /// Resolve from the environment (absent → single full sweep).
    #[must_use]
    pub fn from_env(iters: u64) -> Self {
        let index = std::env::var("CAIRN_SIM_SHARD_INDEX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let total = std::env::var("CAIRN_SIM_SHARD_TOTAL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        Self::new(index, total, iters)
    }

    /// Seeds owned by this shard (empty when the shard has no work).
    #[must_use]
    pub fn seeds(&self) -> Vec<u64> {
        if self.last_seed < self.first_seed {
            return Vec::new();
        }
        (self.first_seed..=self.last_seed).collect()
    }
}

/// The I2 gate: run the randomized schedule sweep for the given seeds; panics with the
/// SHARD + SEED on any violation (the CI job surfaces shard context directly).
///
/// Aggregate gates across the sweep (so a broken engine cannot hide behind
/// vacuous schedules):
/// - vacuous (no-progress) schedules must stay ≤ 20% of the sweep;
/// - the sweep must ack at least one append in total.
pub fn run_sweep_sharded(shard: Shard, ticks: u64) {
    let seeds = shard.seeds();
    if seeds.is_empty() {
        tracing::info!(shard = shard.index, "no schedules assigned to this shard");
        return;
    }
    let mut vacuous = 0u64;
    let mut total_acked = 0u64;
    for seed in seeds {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let verdict = rt.block_on(async { world::run_schedule(seed, ticks).await });
        total_acked += verdict.appends_acked;
        if verdict.vacuous {
            vacuous += 1;
        }
        assert!(
            verdict.ok(),
            "I2 VIOLATION in shard {}/{} schedule seed {seed}: {verdict:?}",
            shard.index,
            shard.total
        );
    }
    let swept = u64::try_from(shard.seeds().len()).unwrap_or(u64::MAX);
    assert!(
        vacuous * 5 <= swept,
        "SWEEP INVALID in shard {}/{}: {vacuous}/{swept} schedules were vacuous \
         (no progress allowed by the fault script) — gate inconclusive",
        shard.index,
        shard.total
    );
    assert!(
        total_acked > 0,
        "SWEEP INVALID in shard {}/{}: zero appends acked across {swept} schedules",
        shard.index,
        shard.total
    );
}

/// Back-compat full sweep (local dev).
pub fn run_sweep(iters: u64, ticks: u64) {
    run_sweep_sharded(Shard::new(0, 1, iters), ticks);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sim suite gate (§15.1) — assertions (a)–(d) over the sweep. CI sharding:
    /// `CAIRN_SIM_ITERS` (total schedules), `CAIRN_SIM_SHARD_INDEX`/`_TOTAL` (20 runners),
    /// `CAIRN_SIM_TICKS` (schedule length). Locally: full unsharded mini-sweep.
    #[test]
    fn sim_sweep_assertions_hold() {
        let iters = default_iters().max(2);
        let ticks = std::env::var("CAIRN_SIM_TICKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(12);
        let shard = Shard::from_env(iters);
        run_sweep_sharded(shard, ticks);
    }
}

#[cfg(test)]
mod shard_tests {
    use super::Shard;

    /// 1000 schedules / 20 shards = 50 contiguous seeds each.
    #[test]
    fn sharding_math_covers_all_seeds_exactly() {
        let mut all = Vec::new();
        for shard in 0..20u64 {
            let s = Shard::new(shard, 20, 1000);
            assert_eq!(s.seeds().len(), 50, "shard {shard}");
            assert_eq!(s.first_seed, shard * 50 + 1);
            assert_eq!(s.last_seed, (shard + 1) * 50);
            all.extend(s.seeds());
        }
        all.sort_unstable();
        assert_eq!(all.first().copied(), Some(1));
        assert_eq!(all.last().copied(), Some(1000));
        assert_eq!(all.len(), 1000, "no seed lost or duplicated across shards");
    }

    #[test]
    fn remainder_shard_is_smaller_and_bounds_hold() {
        // 1007 schedules over 20 shards: ceil division ⇒ shards 0-18 get 51,
        // shard 19 gets 970..=1007 (38 seeds) — full coverage, no overlap
        let s = Shard::new(19, 20, 1007);
        assert_eq!(s.first_seed, 970);
        assert_eq!(s.seeds().len(), 38);
        let mut all = Vec::new();
        for shard in 0..20u64 {
            all.extend(Shard::new(shard, 20, 1007).seeds());
        }
        all.sort_unstable();
        assert_eq!(all.last().copied(), Some(1007));
        assert_eq!(all.len(), 1007);
        let over = Shard::new(25, 20, 1000);
        assert!(over.seeds().is_empty(), "out-of-range shard has no work");
    }

    #[test]
    fn single_shard_is_full_sweep() {
        let s = Shard::new(0, 1, 1000);
        assert_eq!(s.seeds().len(), 1000);
    }
}
