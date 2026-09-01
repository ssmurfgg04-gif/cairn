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

## Real-corpus ingest (2026-08-31, same host)

Real studio-grade media, NOT synthetic — source files deleted after measurement;
JSON report: [`docs/real-corpus-report.json`](real-corpus-report.json).
Repro: `bash scripts/fetch-real-corpus.sh /tmp/cairn-real-corpus real-corpus-report.json`.

| metric | result | notes |
|---|---|---|
| Corpus | **525.3 MiB, 407 files** | Tears of Steel 720p (372 MB, Blender), Sintel trailer 720p, 405 real UCF101 .avi clips pulled from a Hugging Face LFS-hosted dataset |
| Ingest throughput on real media | **670 MiB/s** | full chunk+hash pipeline over all 407 files |
| Chunks (real bytes) | 483 chunks | avg ≈ 1.1 MB on dense camera motion; 483 unique — cross-file dedup 0.0% between unrelated footage (honest negative: distinct takes share no 1–16 MB regions) |
| Save-shaped mutation reuse (REAL footage) | **97.1% chunk-hash identity, +2 chunks re-uploaded** | 64 KB re-render window + 64 KB append against the 372 MB real movie — the engine would move ~1.7 MB of a 372 MB "save", not the file |

Reading it: on real editors' media, a realistic save reshapes only the edited
region + tail — chunk-hash reuse ≫ the 70% acceptance gate, and content
addressing turns "upload the file" into "upload the delta" without any diff
format. Container formats (gzip'd .prproj / zip'd .drp) are excluded from these
reuse guarantees until the flag-gated normalization soaks — see STATUS.md.

## COLD-FETCH first byte (WO6-4, 2026-09-01)

**Definition.** A device that has NEVER seen a chunk (fresh process, empty
client store) fetches one stored chunk through the REAL download path:
`GetDownloadUrl` (presign RPC) → presigned GET, body streamed. The number is
the time to the FIRST BODY BYTE (presign + server round trip + first data).
It is the hydration-latency floor every cold open pays before any bytes flow.

**Instrumentation.** `GrpcPlane::measure_cold_fetch` (crates/cairn-sync) +
`cairn-x cold-fetch --home <device> --hash <chunk> --iters N` (p50/p95/max
reported). Wire test: `crates/cairn-server/tests/cold_fetch.rs` drives the
same fn against an in-process server. In the soak, gate S4 picks the LARGEST
stored chunk and asserts the body byte-count equals the chunk size.

| environment | first byte p50 | first byte p95 | notes |
|---|---|---|---|
| LocalFs server, loopback, 16 MiB chunk (dry-run soak, 150–200 MiB corpus, kill−9-resumed state) | **3.87–4.28 ms** | 5.15–8.03 ms | 5 runs, fresh device C per run; body byte-count verified |
| MinIO S3 backend, loopback (CI `soak-s3` job, 500 MiB corpus) | measured on CI | measured on CI | presigned GET through the real SigV4 wire; number lands in the run log |
| Cloud bucket (user's CAIRN_S3_*) | — | — | pending real credentials — HUMAN-GATE; `just soak-5gb` prints it |

**Honest coldness caveat.** "Cold" here = fresh process + empty client state.
The server's OS page cache may hold the chunk (loopback + local disk); on a
real bucket, the first fetch is a genuine network+bucket round trip. Where
privileges allow, run `CAIRN_SOAK_DROP_CACHES=1 bash scripts/soak.sh` — the
script drops the page cache (root/sudo) and says so in the log; when it
cannot, it prints the limitation instead of pretending.

**I1 provenance (WO6-5).** The Windows I1 number (first 2 MiB through the
CfAPI callback, gate < 50 ms) is environment-sensitive: 16.32 ms on a calm
windows-latest runner (2026-08-31, run 33478971953) vs 55.46 ms on a
contended one (2026-09-01, run 33497721283). The CI gate therefore takes the
BEST of 3 fresh-placeholder hydrations (capability, not contention) and
prints every sample. Budgets: Linux FUSE-parity burst variant → WO6-5.
