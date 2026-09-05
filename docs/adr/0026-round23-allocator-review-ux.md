# ADR-0026: Round 23 — mimalloc, the Frame.io review surface, the honest remainder

Date: 2026-09-05 · Status: accepted · Scope: cairn-cli, cairn-store, cairn-review assets

## Decision

### 1. mimalloc global allocator (the shipped binary)

`cairn-cli` (CLI + daemon + server in one) binds `mimalloc::MiMalloc` as the
global allocator. Johal's Rust-1.85 memory round: per-thread heaps, 8B
header, 42 ns/128B alloc vs 68 ns, p99 89 ns vs 142 ns — and the quiet
killer, 12-week heap fragmentation 20.7% vs the ~67% class: a long-lived
daemon stops bleeding RSS. Our allocation shape is exactly the cited winner:
many small, worker-thread-local allocations (presence, notes, SQLite rows,
128B-class payloads). 256 KB per-thread control is mimalloc's default; no
tuning knobs taken (measure first).

### 2. The Frame.io review round — frontend-carried, contract-frozen

Frame.io V4's reviewer surface (help.frame.io 12833113 / 9105251 / 9105278):
the gap was UX, not throughput. Implemented in `review.js/html/css`:

- **click comment → jump playhead**: the whole note row is the seek target
  (`.seekable` hover state), not just the timecode button.
- **search + person + #tag filter + sort**: text search over body/author,
  one `#tag` chip bar built from the notes' own hashtags, timecode↔newest
  sort toggle.
- **Copy Link to timestamp**: per-note link button copies `?t=&v=` deep
  links; the portal seeks on load.
- **filtered CSV export**: the `csv` button exports exactly the filtered,
  sorted, active-version view.

All frontend-carried because the note sidecar's id is `blake3(anchor‖body‖author)`
— a FROZEN content-derived contract shared with the CLI/OTIO toolchain
(ADR-0018). Range brackets (I/O in-out), pin + annotation canvas, and
per-note internal/public visibility REQUIRE persisted fields; that is a
protocol extension (note-shape v2 + migration + CLI/interop updates), and is
recorded as the named follow-up, not smuggled in as body-text hacks that
would corrupt mechanical-note parsing and every existing id.

### 3. Store pragmas (sqlite-kit convention)

`cache_size=-32000` (32 MiB page cache — the header/serve path reads
multi-MB blobs; the default ~2 MiB thrashes) + `temp_store=MEMORY` on the
client store open. WAL + busy_timeout + NORMAL were already in place.

## Rejected / deferred (honest, with reasons)

- **chunkrs / Bytes zero-copy / NEON CDC swap**: our FastCDC cut points are
  protocol-frozen (CHUNKER_VERSION; ADR-0003 differential pins
  bit-identical cuts). Swapping chunkers changes every chunk identity — a
  protocol break, not an optimization. Per-file parallelism already exists
  via the ADR-0025 offload lane. `Bytes` in read paths is a real but broad
  refactor (CAS/manifest/hydrate signatures) — deferred with the layer audit.
- **QUIC/BBR/quinn + zblob/bao transport swap**: the plane is tonic-over-TLS
  + presigned S3 GET/PUT; a QUIC re-plumb is a wire-protocol change touching
  server, client, and every test harness. The p2p swarm (17780/17781) is the
  right future home for verified-streaming (bao range sets) — designed,
  deferred.
- **zstd 3→7 / super-blocks (packt)**: compression is wire-only and -3 is a
  deliberate speed/size tradeoff at 859 MiB/s ingest; raising the level
  trades ingest CPU for wire bytes on a 200 Mbps-class budget where neither
  binds. Revisit when proxy pipelines (ADR-0016-class) become the bottleneck.
- **PGO/BOLT, UPX, distroless, seccomp, cargo-vet/shear**: tail-percentile
  packaging/CI work — recorded, not load-bearing for the beta.
- **FUSE `big_writes`/`auto_cache` mount flags**: the fuser mount options
  need a live /dev/fuse leg to validate honestly; queued for the
  fuse-mount-live runner round rather than blind-set.
