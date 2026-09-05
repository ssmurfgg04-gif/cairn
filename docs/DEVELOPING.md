# Developing on Cairn — the Rust setup map

One page, one answer per item of the "top-10 Rust setup" checklist, plus the
runtime-discipline decisions behind them (ADR-0025). If you are setting up a
new machine, read this top to bottom; everything it references is committed.

## 1. Toolchain + components

`rust-toolchain.toml` pins the channel (1.98.0) and installs the essentials
automatically on first build: `clippy`, `rustfmt`, `rust-analyzer`,
`rust-src`. `rustup update` moves the default toolchain; the repo pin moves
deliberately per round (CI pins per-job — see `tl-merge-gate`'s 1.98.0).
`rust-version = "1.85"` in the workspace manifest is the MSRV floor.

## 2. `cargo check` for fast feedback

`just check-fast` = `cargo check --workspace` — 2–3× faster than a build
(no codegen/link), exactly what you want while iterating. VSCode runs it
implicitly via rust-analyzer (see #9). `just build` / `just test` when you
need the binaries.

## 3. Faster linker (optional, opt-in)

`.cargo/config.toml` ships a **commented** `[target.x86_64-unknown-linux-gnu]`
block: `linker = "clang"` + `-fuse-ld=lld` (or `mold`). Uncomment it on hosts
that have the tools — `cairn-cli`'s link time drops hard. It is opt-in
because the default `cc` path must build everywhere (CI included).

⚠ **RUSTFLAGS precedence**: setting `RUSTFLAGS` in your env *replaces* the
config flags. The config's `--cfg tokio_unstable` is load-bearing (tokio's
`io-uring` feature compile-errors without it) — any manual `RUSTFLAGS` must
include it or the build breaks loudly (that's the safety net, not a bug).

## 4. sccache

CI uses `Swatinem/rust-cache@v2` (per-job target + registry cache), which
already banks most of the win for this repo's CI shape; switching the whole
matrix to sccache is a re-plumb with breakage risk for marginal gain —
recorded in ADR-0025. For local cross-project caching:

```
cargo install sccache
export RUSTC_WRAPPER="$(which sccache)"
sccache --start-server        # stats: sccache --show-stats
```

The exact CI snippet (Mozilla-Actions/sccache-action + GHA cache backend)
lives in that action's README; adopt it repo-wide the day rust-cache stops
paying for itself (e.g. when the workspace outgrows the 10 GB cache cap).

## 5. Profile settings

Committed in the workspace `Cargo.toml`:
dev = opt-level 0, `debug = false`, **incremental on** (CI pins
`CARGO_INCREMENTAL=0`); release = **opt-level 3, fat LTO,
`codegen-units = 1`, `panic = "abort"`, `strip`**. Notes:
- cargo forces unwind for test/bench profiles — `cargo test` (incl.
  `--release`) is unaffected; release *doctests* are the one unsupported
  combination (CI never runs them).
- fat LTO + single CGU make release builds slower — that's the trade, pay it
  for the shipped binaries, never for the dev loop.

## 6. Zombie dependencies

`just machete` (`cargo install cargo-machete`, then
`cargo machete --workspace`). Run it in review when a dependency looks
stale; keep the lockfile honest (`Cargo.lock` is committed).

## 7. Compile-time analysis

`just timings` = `cargo build --workspace --timings` → interactive HTML at
`target/cargo-timings/cargo-timing.html`. CI runs the same thing on every
push and uploads the report as the `cargo-timings` artifact — compare two
runs to find the crate that regressed the build.

## 8. Workspaces

Already a 17-crate workspace (`crates/*`, resolver 2) — that's why
`cargo check --workspace` parallelizes the way it does. The two standalones
are deliberate: `cairn-app` (Tauri's dep tree, ADR-0022) and `cairn-x/fuzz`
(nightly cargo-fuzz).

## 9. IDE

`.vscode/{settings,extensions}.json` are committed:
rust-analyzer with **clippy on save** (`rust-analyzer.check.command`) at the
same `-D warnings` bar CI enforces, plus the TOML extension. The
rust-analyzer binary comes from #1's toolchain components.

## 10. Faster tests

CI runs the suite via **nextest** (`taiki-e/install-action@nextest` —
process-per-test isolation, better failure surfacing). Locally:
`just test` (nextest if installed) or `just test-full`
(plain `cargo test --workspace`). Install locally with
`cargo install cargo-nextest --locked`.

---

## The runtime contract (ADR-0025, read before touching async paths)

- **CPU work never runs on tokio I/O workers.** Hash+chunk goes through
  `cairn_sync::offload::hash_stream_owned` (rayon lane + semaphore + oneshot,
  PostHog pattern); small work stays inline. If you add a CPU-heavy step to
  an async fn, route it through the lane or justify the exception.
- **Blocking reads don't belong in async fns.** Use `tokio::fs::…` (rides
  the io_uring driver when armed, blocking pool otherwise) or
  `Cas::get_async`. The FUSE sync path legitimately stays sync.
- **io_uring is unstable-by-choice.** tokio `io-uring` feature +
  `--cfg tokio_unstable` from `.cargo/config.toml`, runtime-probed with
  automatic fallback. Cargo.lock pins tokio 1.53.1 — the cfg is a contract;
  bump tokio deliberately, with the suite green.
- **Mailboxes are bounded.** New channels get an explicit capacity + a
  saturation story (see the watcher mailbox in `projects.rs`: 512,
  back-pressure at the sender, warn at 80 %).
- **SQLite: one writer lane, pooled readers.** Writes go through the store's
  shared connection; read-heavy surfaces use `HeaderCache::with_read_pool`
  (r2d2, 8 wide, `try_get` + shared-conn fallback). Keep
  `r2d2_sqlite` on the 0.25.x line that matches our rusqlite 0.32.
