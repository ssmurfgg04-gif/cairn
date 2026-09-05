# ADR-0025: Round 22 — runtime discipline (PostHog pattern, io_uring, pooled readers)

Date: 2026-09-05 · Status: accepted · Scope: cairn-sync, cairn-store, cairn-cli, build config

## Context

The Round 21 memory audit (ADR-0024) left a sibling problem open: the daemon's
I/O runtime also *executed* the CPU- and file-heavy parts of ingest. Three
concrete pathologies, all measured or cited in `docs/BENCHMARKS.md`:

1. **CPU on tokio workers.** `process_file` ran FastCDC+BLAKE3 (~0.8–1.2 GiB/s)
   inline in an async fn — a 100 MiB save parked an I/O worker ~130 ms, and
   concurrent saves froze every latency-sensitive task (headers, presence,
   dashboard). This is exactly PostHog's flags-service failure ("Untangling
   Tokio and Rayon in production", 2026-04: two runtimes both sized to "all
   cores", `par_iter` in handlers, p99 spikes of 2.5 s → 94 ms after the fix).
2. **Blocking file reads in async fns.** `std::fs::read` / `Cas::get` on the
   hydration path did the same for disk I/O, at multi-MiB scale per chunk.
3. **One serialized SQLite lane for reads.** WO6-5 (burst bench, 2026-09-01):
   32 concurrent opens serialize on the store's single
   `Mutex<Connection>` — header-serve lockstep p95 330 ms on the shared-runner
   burst. The interim hand-rolled reader pool fixed the shape but had its own
   pathology (unbounded lazy top-up: every drain event appended a connection).

The one *unbounded* queue in the daemon (the watcher→sync mailbox) was the
last "strictly bounded mailboxes" gap (the async-vs-concurrent trap: an
unbounded mailbox converts a producer burst into unbounded memory + silent
queueing, instead of backpressure).

## Decision

### 1. PostHog-pattern CPU lane (`cairn-sync/src/offload.rs`)

- Work below **8 MiB hashes inline** (dispatch costs more than the work;
  PostHog's "<200 flags stay sequential", scaled to our 4 MB chunk profile).
- Bigger buffers **move to the rayon pool** via `rayon::spawn` + a
  `tokio::sync::oneshot` — the I/O worker is released the moment the buffer
  is handed over, and the result (plus the buffer, unmodified) comes home.
- A **`Semaphore` sized to the CPU lanes** caps in-flight offloads — a
  dirty-file burst of 100 files queues at the valve (their "pressure valve")
  instead of burying rayon under unbounded work.
- **Thread budget at boot**: rayon pool = all logical cores, tokio
  `worker_threads` = half (min 2) — the PostHog budget. Both pools at full
  width is the oversubscription that produced their 2.5 s p99; the semaphore
  keeps the sum honest regardless.
- Pinned by `offload_keeps_the_io_worker_free`: on a **single-worker**
  runtime, a 96 MiB ingest must not delay an unrelated 20 ms timer beyond a
  3.5×-slack bound — the pre-lane code fails that by ~100 ms.

**Rejected:** per-chunk `par_iter` inside the hash pass. CDC boundaries are
cut sequentially by construction (the Gear hash carries state across bytes),
the whole-stream BLAKE3 is single-pass, and the parallelism unit that matters
is the FILE (many dirty files / devices), which the lane already provides.
**Rejected:** `spawn_local` for handler children (johal tip #2). It requires
a `LocalSet`-driven runtime restructure for no measured win here; the
semaphore + bounded mailboxes deliver the same discipline.

### 2. io_uring, the honest unstable path

- Workspace tokio is now `features = ["full", "io-uring"]` on the lockfile's
  1.53.1, with `--cfg tokio_unstable` set for **every** build via
  `.cargo/config.toml` (the feature hard-errors without the cfg — a loud
  guard, not a silent contract).
- We do **not** call uring APIs ourselves: `tokio::fs::read` (engine file
  reads, `Cas::get_async`) routes through tokio's uring driver when armed —
  **runtime-probed with automatic fallback** to epoll/blocking-pool on
  kernels or runtimes without ring support (verified: this sandbox's 5.10
  kernel sits below the materials' 5.15 comfort line and the suite is green
  either way; Windows/macOS compile clean — the tokio `io-uring` dependency
  is `cfg(all(tokio_unstable, target_os = "linux"))`-gated in tokio itself).
- `enable_all()` arms the driver automatically (tokio 1.53:
  `cfg(tokio_unstable, feature = "io-uring", feature = "fs", linux)`), so the
  CLI's existing runtime builder needs no uring-specific code.

**Rejected / deferred:** SQPOLL — tokio 1.53 exposes no
`uring_setup_sqpoll` builder knob; per-thread rings are internal. Windows
`IoRing` — file-I/O-only today, no Rust runtime support; revisit when tokio
grows a Windows ring driver. DPDK/AF_XDP, CXL, computational storage — out
of scope for a file-sync daemon (Pareto gate from the round brief).

### 3. r2d2 reader pool (the WO6-5 fix, done properly)

`HeaderCache::with_read_pool` now carries an **r2d2 pool** of
`PRAGMA query_only` connections (busy_timeout 5 s): bounded at **8** readers
(production width — FUSE mount, daemon run-loop, and the burst bench all
agree), health-checked, capped, reused — replacing the hand-rolled
pop/push/top-up Vec. `serve` uses **`try_get`** (non-blocking): a saturated
pool degrades to the shared connection instead of queueing — the pool
remains an optimization, never a dependency (pinned by
`saturated_pool_falls_back_to_the_shared_connection`).

Version pin note: `r2d2_sqlite` **0.25.x** is the release line built against
our rusqlite 0.32 — any other line duplicates the rusqlite crate and breaks
the `PooledConnection ↔ Connection` type identity (probed empirically:
0.31→rusqlite 0.37 … 0.25→0.32).

### 4. Bounded watcher mailbox + saturation gauge

The watcher feed (the daemon's one unbounded channel) is now
`mpsc::channel(512)`: the forwarder thread **back-pressures** via
`blocking_send` when the budget is spent, and the consumer **warns at ≥80 %
saturation** (with a drain notice) so the budget is spent loudly. 512 covers
the worst single rename storm a project throws at once.

### 5. Build/devx surface (the "top-10" list)

- `rust-version` 1.80 → **1.85**; `rust-toolchain.toml` now installs
  `clippy`, `rustfmt`, `rust-analyzer`, `rust-src` for every contributor.
- Dev profile: **incremental on** (CI pins `CARGO_INCREMENTAL=0`), no debug
  info. Release profile: **opt-level 3, fat LTO, codegen-units 1,
  panic = "abort", strip** (cargo forces unwind for test/bench profiles —
  `cargo test` incl. `--release` is unaffected; release doctests are the one
  unsupported combination, and CI never runs them).
- `.cargo/config.toml`: the tokio cfg + a commented, ready-to-uncomment
  clang+lld/mold linker block (kept opt-in: the default `cc` path must build
  everywhere; see the RUSTFLAGS-precedence warning in that file).
- CI: a `timings` job uploads the `cargo build --timings` HTML report per
  push; nextest was already the CI runner. `.vscode/` recommends
  rust-analyzer with **clippy on save**.
- `docs/DEVELOPING.md` maps every top-10 item to the repo's answer,
  including the sccache decision (kept: Swatinem/rust-cache in CI — switching
  the whole matrix to sccache is a re-plumb with CI-breakage risk for a
  win rust-cache already mostly banks; sccache remains the documented local
  option with the exact CI snippet for when the team outgrows rust-cache).

## Consequences

- I/O workers no longer park on hash CPU or multi-MiB blocking reads: the
  latency-sensitive surfaces (I1 header serves, presence, dashboard) stop
  inheriting ingest load (pinned by the offload timer test).
- The burst lockstep series should drop with the 8-wide r2d2 pool; the gated
  CfAPI-parity series keeps its 20× headroom (BENCHMARKS.md carries the
  before/after).
- The tokio unstable cfg is a real (accepted) contract: Cargo.lock pins
  1.53.1, and any `RUSTFLAGS` env override that drops `--cfg tokio_unstable`
  fails loudly at tokio's compile_error! gate rather than silently
  downgrading to no-uring.
- Engine ingestion semantics are unchanged: the offload lane is bit-identical
  (`big_buffers_offload_identically` pins spans/hashes/stream/file hash),
  the raw-size idempotency key and the raw-file header carve are preserved
  (captured before the buffer moves), and a rayon panic degrades to an
  ordinary ingest error that re-dirties the file for the next pass (I2).
