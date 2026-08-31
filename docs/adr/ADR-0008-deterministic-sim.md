# ADR-0008: Deterministic simulation — in-house seeded scheduler + fault hooks

Date: 2026-08-31 · Status: Accepted

## Context
Spec §15.1 mandates deterministic simulation (suggests madsim or shuttle) as the enforcement
mechanism for I2. madsim virtualizes tokio globally (invasive runtime swap); shuttle explores
task schedules by replacing the executor. Both impose runtime-wide abstractions across every
crate and pin us to their release cadence; neither is in the dependency budget of the v4 core
build (2-core/4GB build environment).

## Decision
`cairn-sim` implements a small deterministic harness around the real engine code:
1. The engine and server are written against narrow traits (`SystemClock` for time) and all
   multi-step client operations expose numbered fault hooks (the §9 steps: `before_batch_exists`,
   `after_presign`, `mid_upload(n)`, `after_complete_upload`, `before_manifest_put`,
   `before_journal_append`, `after_journal_append`, ...).
2. The harness runs 2–4 real engine instances over real SQLite (temp files, real WAL) against
   one in-process server, driven by a seeded `StdRng` that chooses: which device acts, injected
   crash (abort device task + reopen = crash semantics equivalent for SQLite WAL), network
   partition (store/auth wrapper returns injected 5xx/timeouts), lease expiry timestamps, GC and
   fold concurrency timing.
3. Assertions (a)–(d) from §15.1 run every schedule. 1,000-schedule runs are the nightly CI job;
   the default test run executes a smaller deterministic sweep locally.
4. Wall-clock-sensitive logic (AIMD dynamics) is property-tested separately; sim uses injected
   timestamps so logical behavior is fully deterministic.

## Rationale
Crash semantics for SQLite (W)AL — commit either happened or did not — are preserved because the
sim kills real processes' worth of state (abandons handles, reopens files). Real `kill -9` of
whole subprocesses is additionally covered by the `cairn-x` fault harness (§15.4, M1/M3 ACs).
This is recorded as a deviation from the madsim/shuttle suggestion; the §15.1 assertions are
implemented verbatim.

## Consequences
- No runtime-wide virtualization; the engine must keep its fault hooks (they are also the
  metering/trace seam, so they earn their place).
- Determinism depends on keeping RNG use inside the harness only (production code never RNGs
  except full-jitter backoff, which the harness injects).
