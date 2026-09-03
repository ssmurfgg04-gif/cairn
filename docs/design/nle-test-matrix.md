# NLE human-gate test matrix — DaVinci Resolve · Blender · Premiere Pro

Status: plan (WO6-10 round) · Scope: real-Windows-studio-box human gates, companion to
the automated `windows-cfapi-roundtrip` CI job (which already proves placeholder
registration, callback hydration, and I1 through the filter on a VM).

Why this exists: the beta studios are Windows-first and their daily driver is an NLE,
not Notepad. The automated roundtrip proves the CfAPI machinery; it cannot prove that
an artist's actual save/open loop stays correct and fast for a week. This matrix is the
human-gate contract for that loop. Every row is executed on a real Windows box with the
cairn daemon attached and `RUST_LOG=info` captured to a file.

## Behavior model per NLE (what the mount must survive)

- **Premiere Pro (.prproj)** — project is a zip-wrapped XML-ish document. Auto Save
  (default every 15 min, configurable to 1 min) writes BOTH the live project file and
  timestamped backups in `Adobe Auto-Save` — meaning: rapid successive full-file saves
  of the SAME path, plus sibling files the mount must not misinterpret. Adobe documents
  that Auto Save also saves the current project (not only backups), so expect a save
  storm on the live path while backups appear next to it.
- **DaVinci Resolve (.drp)** — live-save model: the project DB persists continuously;
  File ▸ Export exports a `.drp` bundle (multi-entry zip). Treat `.drp` as opaque bytes
  (Cairn already rejects zip-arm normalization loudly — ADR round-4); the sync-relevant
  artifacts are export/import events (large, whole-file writes) and media-cache churn.
- **Blender (.blend)** — "Compress file" is OFF by default (devtalk: compression makes
  saves slower and older Blender versions can't open compressed). Uncompressed .blend =
  raw binary with a 12-byte header; autosave/versions write `.blend1`/`.blend2`
  rotations next to the file. A compressed .blend is gzip-wrapped (inner `BLENDER-v`
  magic) — the exact container shape the BMW27.blend round-trip test proves.

## Gate instrumentation (run before, during, after each matrix)

- `cairn doctor` healthy; `cairn status --json` captured per row
- Metrics: `cairn_hydration_first_byte_ms` (I1), `sync_propagation_p95`, outbox depth
- Byte-identity oracle: BLAKE3 of every file in the tree before/after each row
- Leakage oracle: daemon log shows ZERO journal ops for paths outside the attached root

## The matrix (every cell: steps → expected)

| # | NLE | Row | Steps | Expected / measured |
|---|-----|-----|-------|---------------------|
| H1 | Premiere | cold open | Placeholder-only project; double-click .prproj in Explorer | Premiere opens; first 2 MiB through CfAPI ≤ I1 budget (2 attempts logged); NO full media hydration (watch metrics: only header-cache-sized fetches) |
| H2 | Premiere | save storm | Enable Auto Save @ 1 min, edit 10 min | Every save syncs delta-only (journal: 1 upsert/save, chunk-reuse > 70% on consecutive saves); no `Adobe Auto-Save` sibling storm errors |
| H3 | Premiere | scrub | Open timeline, scrub 5 min across braw/prores on placeholders | Playback from local cache/scratch; hydration never blocks UI thread > 500 ms; pins honored (pinned media never evicted) |
| H4 | Resolve | export .drp | Attach root, open project, File ▸ Export .drp to mounted root | Whole-file write syncs; opaque-byte policy holds (no normalization attempt in log); re-import on device B byte-identical |
| H5 | Resolve | live-save | 30 min editing session with project server backed by mounted root | Continuous small writes converge; `sync_propagation_p95` < 5 s; no conflict copies (single editor) |
| H6 | Blender | cold open | .blend placeholder, open + wait for linked media placeholders | Header loads < 1 s perceived; viewport proxies load progressively; BLAKE3 identity after full load |
| H7 | Blender | save + rotate | Ctrl+S × 20 (default 2-min autosave, .blend1 rotation) | Each save = delta upsert; .blend1/.blend2 rotations sync as normal files; chunk-reuse > 70% |
| H8 | Blender | compressed .blend | Save with Compress=ON, sync, open on device B | gzip-container path: inner payload chunks plain zstd-3; device B opens byte-identical (BMW27 proof at production scale) |
| H9 | all | conflict | Two devices save same .prproj/.blend within window | ONE conflict copy, spec naming `name (conflict — device — date).ext`; both versions recoverable |
| H10 | all | offline week | Disconnect 5 days, NLE used daily; reconnect | Outbox drains; no lost saves; journal dedupe makes retries invisible; doctor stays healthy |

Rows H1–H3 are the Premiere beta blockers; H4/H5 Resolve; H6–H8 Blender; H9/H10 ship
gates for all three.

## GitHub scan result (prior art, 2026-09-01)

- No public NLE×cloud-VFS test matrix found to adopt wholesale (searched CfAPI /
  FileProvider / FUSE + video-editing combos). LucidLink's public docs describe the same
  placeholder+on-demand-fetch pattern we implement but publish no test contract — this
  matrix is the open version of that missing artifact.
- `awesome-video` and `cloud-storage` topic lists on GitHub are link aggregators; nothing
  executable. The `nextcloud/desktop` vfs/cfapi patterns remain the best public CfAPI
  reference (already vendored + credited in THIRD_PARTY.md).

## Sources (behavior model)

- Adobe Helpx: "Configure Auto Save preferences" (Auto Save writes backups; community
  threads confirm the current project is saved too)
- Blender devtalk: "Why is compress file = off by default" (default OFF, rationale)
- blenderartists / blender.SE: compressed .blend = gzip wrapper, openable cross-version
- Blackmagic forum / davinciresolveclub: Resolve live-save + .drp export/backup flow

## Instrumentation bundle (round 12) — the "report back" protocol

`scripts/nle_matrix_collect.py` is the collector: it snapshots `cairn doctor`
+ `cairn status --json`, hashes the whole project tree before/after each
confirmed H-row (the byte-identity oracle), pulls
`cairn_hydration_first_byte_ms` / `sync_propagation` lines from the daemon
log, and writes one JSON. It is read-only against the project tree and never
needs admin rights.

Studio procedure (the I1 human-gate contract):

1. Attach the project on the Windows box; run the daemon with
   `RUST_LOG=info` captured to a file.
2. `python scripts/nle_matrix_collect.py --project <id> --log <daemon.log>
   --box "<CPU/GPU/RAM/NVMe>" --nle premiere`
3. Work the H-rows in the NLE; after each row run the collector again with
   `--confirm-row H2` (repeatable) so the row pairs the human action with
   the objective measurements.
4. Send back the JSON (issue tracker, email, anything) — results land in
   `docs/nle-matrix-results/` and update `docs/BENCHMARKS.md` with live
   numbers.

### Minimum hardware spec for a meaningful run

The I1 claim is hydration speed under real access patterns, so the box must
not be the bottleneck before Cairn is:

- CPU: 6+ physical cores at 3 GHz+ (NLE decode + daemon hydration in
  parallel)
- RAM: 32 GB (NLE comfortable, cache not thrashed)
- NVMe: 1 TB free, 2 GB/s+ read (placeholder hydration target)
- Network: 100 Mbps+ to the object store (the sync path)
- OS: Windows 10 22H2 / 11, NLE: Premiere Pro 2024+, Resolve 19+, Blender 3.6+
- The 5 GB project media should EXCEED local free cache so hydration is
  actually exercised (otherwise you measure the local disk, not Cairn)

Anything weaker still produces a valid report — the JSON captures the box
description; we grade against the claim, not the hardware.
