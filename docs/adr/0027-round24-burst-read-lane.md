# ADR-0027: Round 24 — the BURST read lane (CAS verify offload + atime debounce)

Date: 2026-09-05 · Status: accepted · Scope: cairn-store

## Context

ADR-0025 fixed the *ingest* side of the runtime discipline story: hashing and
multi-MiB reads moved off the tokio workers (rayon + oneshot, the PostHog
shape), the header cache rides an r2d2 reader pool, the watcher mailbox is
bounded. The *serve* side — the path WO6-5 BURST actually drives
(`CairnFs::serve_header` + `serve_read` → `Cas::get_async`) — kept two of the
three Round 22 pathologies, both visible in `docs/BENCHMARKS.md`:

1. **Verify CPU on the I/O worker.** `get_async` read the chunk via
   `tokio::fs::read` (the io_uring lane) and then ran `Hash::of(&bytes)`
   *inline on the same async worker*. BLAKE3 is fast (~5 GiB/s) but not free:
   a 2 MiB header read parks the worker ~0.4 ms, and a 32-worker BURST turns
   that into the same p99 lockstep PostHog described — the worker that should
   be issuing the next ring read is busy hashing the previous one.
2. **Every read ends in a serialized write.** `get`/`get_async` call
   `touch()` → `UPDATE blobs SET atime=...` behind the store's single writer
   mutex. Under BURST (32 workers re-opening the same 32 files), that is a
   pure write storm: the atime value only feeds LRU eviction, which runs on
   the 24 h job cycle — the writes deliver nothing the policy can observe.

The third candidate on the round-24 list — SO_REUSEPORT multi-accept in
`cairn-server` — was measured against its cost and rejected (below).

## Decision

### A. Big buffers verify on the rayon CPU lane (PostHog shape, serve side)

`Cas::get_async` now splits by size:

- `bytes >= 256 KiB`: ownership round-trips through the rayon pool —
  `rayon::spawn(move || { hash; tx.send((bytes, hash)) })` + oneshot. Zero
  copies (the buffer is *moved* through the channel and back), the async
  worker never hashes, and a rayon panic surfaces as an honest error instead
  of a hung await.
- `bytes < 256 KiB`: hash inline. The PostHog rule runs both directions:
  small work stays inline when the round-trip costs more than the work.
  256 KiB at ~5 GiB/s ≈ 50 µs ≈ the spawn+oneshot round-trip — the break-even.

The threshold constant (`CPU_LANE_MIN_BYTES`) is deliberately local to
`cairn-store`: the global rayon pool is installed at CLI boot by ADR-0025's
`init_cpu_lanes()` (rayon = all cores, tokio = half); if that install has not
run (library consumers, tests), rayon's default pool serves the same
correctness with different width.

### B. Lock-free atime debounce (`TouchFilter`)

`touch()` consults a fixed 2048-slot table of `AtomicU64` keyed by the first
8 bytes of the chunk hash before taking the writer mutex:

- first touch of a hash (or any touch ≥ 60 s after the previous one) writes
  `atime` as before;
- re-reads inside the 60 s window skip the serialized write entirely.

Properties, honestly stated:

- **Zero locks, zero allocation, fixed memory** (2048 × 8 B). Two relaxed
  atomics per hit.
- **Collisions** between distinct hashes sharing a slot at worst skip one
  atime refresh — indistinguishable from the window itself for a policy that
  runs daily and already tolerates `min_age_secs` guards (WO6-2).
- **Eviction semantics**: atime accuracy degrades to ≤ 60 s stale. LRU over
  a 24 h cycle cannot observe that; the WO6-2 young-chunk protection
  (min-age guard) uses cutoffs far coarser than the window.
- **put() keeps its atime write** (the ON CONFLICT update is the durability
  record of a fresh chunk, not a re-touch).

### C. Honest rejections (Pareto round)

- **SO_REUSEPORT multi-accept in `cairn-server`**: the daemon is one process
  with a multi-thread runtime; accept() is not the BURST bottleneck (the
  store lane was). Multi-*process* SO_REUSEPORT would fork the SQLite writer
  story and the tray/daemon lifecycle for no measured win. Rejected.
- **Bounded local queues in `cairn-sim`**: audited — the sim is a
  deterministic discrete-event driver over REAL engine instances; it has no
  async mailboxes at all. The bound that mattered landed on the watcher
  (ADR-0025, 512 + blocking_send backpressure). Nothing to do here; noted so
  the next audit does not re-chase it.
- **`tokio_uring::fs` for the CAS read path**: `tokio::fs` already routes
  through the io_uring driver when armed (probed, with fallback — ADR-0025
  probe notes); a direct `tokio-uring` dependency would fork the driver
  lifecycle for zero additional semantics on this path. Rejected.
- **Batched range reads (200-batch) on serve**: `serve_read` is a single
  2 MiB header-range op per open; there is no batch of ranges to coalesce on
  this path. The batching that exists (journal fetch 512, fold 5000) is
  already batched. Rejected as not applicable.

## Testing evidence

- `touch_filter_collapses_retouches_inside_the_window` — first-touch writes,
  in-window re-touches collapse, past-window writes again (time-travel via
  the `now_ms` parameter).
- `get_async_roundtrips_and_detects_corruption` — inline path (small) and
  rayon-lane path (≥ threshold) both round-trip byte-identical; a tampered
  chunk still fails I2 on the lane (`CHECKSUM_MISMATCH` taxonomy code).
- Full workspace suite green after the change (cairn-store 26 incl. 2 new;
  cairn-sync serve/hydrate tests unchanged and green — behavior-identical).
- `cargo clippy --workspace --all-targets -D warnings` and `cargo fmt
  --check` clean.

## Consequences

- The serve path now matches the ingest path's discipline: I/O on the ring,
  CPU on rayon, writes only when they carry information.
- BURST p95 should shed the lockstep component (verify + touch); the gate
  number itself will be re-measured on the dedicated runner, not asserted
  here — the mechanism is the decision, the benchmark is the evidence to
  collect (same honesty rule as ADR-0024/0025).
- A future r2d2 *read* pool for the blobs table remains open if the debounce
  leaves measurable read-side contention — measured first, then pooled.
