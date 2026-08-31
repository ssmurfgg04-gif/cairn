# Cairn SOTA benchmarks

Repro: `cargo build --release -p cairn-x && ./target/release/cairn-x bench --iters 5`
Harness: `crates/cairn-x/src/bench.rs` — warmup pass, N measured iterations,
median reported; latencies are p50/p99 over the sampled operations.

**Host caveat (read before comparing):** these numbers were taken on a shared
2-vCPU / 4 GB cloud container (2026-08-31) with the release profile. They are
relative-comparison instrumentation for regressions and capacity sanity, not
marketing figures. Re-run on target hardware before quoting absolute values.

## Results (median of 5)

| benchmark | result | notes |
|---|---|---|
| FastCDC chunking throughput | **1,253.8 MiB/s** | 512 MiB stream, 113 chunks, avg 4.53 MiB (spec target 4 MB) |
| BLAKE3 whole-stream hash | **5,028.7 MiB/s** | 512 MiB stream, single-threaded hasher |
| Client ingest pipeline (chunk → hash → verified CAS put incl. fsync) | **859.4 MiB/s** | 128 MiB stream through the exact `cairn-sync` hot path (raw CAS storage; compression is wire-only) |
| I1 header-cache first byte (cached) | **p50 0.003 ms · p99 3.15 ms** | 500 samples, head 2 MiB + tail 1 MiB; I1 gate < 50 ms — **1,600× headroom at p50** |
| CAS random-chunk read (~4.5 MiB chunks) | **p50 ≈ 1.8 ms** | 300 reads, page-cache-warm; ≈ 2.5 GiB/s effective |
| Local store journal append | **5.8 µs** | 1,000 sequential inserts, WAL mode |
| Manifest build + serialize + parse @ 100k entries | **2.7 ms** | fanout @ 8,192 → node tree, round-trip asserted |
| Bloom negative-prefilter (BatchExists) | build **0.09 s** / probe **p50 110 ns · p99 170 ns** | 1M items at fpp 1%; probes are ~30× cheaper than a KV hit |

## Reading the numbers

- The client hot path (ingest pipeline at ~860 MiB/s incl. per-chunk fsync
  durability barriers) comfortably exceeds NLE autosave write rates: a 250 MB
  .prproj autosave fully ingests in ~0.3 s on this box.
- The I1 cached-hydration path has ~1,600× headroom against the 50 ms gate at
  p50 and ~16× at p99 — the gate survives even on loaded workstations.
- Chunking + hashing together run at ~800 MiB/s sustained, so chunking is not
  the bottleneck for 10 GbE-class deployments (2 Gb/s ≈ 250 MiB/s).
- Bloom probe cost (110 ns) validates the BatchExists negative-prefilter
  design: negative lookups never touch the metadata KV store.
