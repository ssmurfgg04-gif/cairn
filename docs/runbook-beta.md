# Beta runbook — onboarding 5 studios (M8)

Prerequisites read: SPEC.md, docs/ctl-api.md, docs/runbooks/*.

## Per-studio onboarding (CLI end-to-end)

0. **(Optional but recommended) swarm transport.** On one reachable machine
   (the bay server or any always-on box):
   ```
   cairn signal                       # prints a fresh join code — share it
                                      # ONLY with the editors who may join
   ```
   Then each editor's daemon joins with the code the host shared:
   ```
   cairn daemon --swarm-signal <host>:17780 --swarm-join-code <code>
   ```
   Effect: block hydration goes peer-first (LAN-speed blocks before cloud
   egress — the big media caches warm from the bay, not the bucket). A
   daemon started WITHOUT a code cannot join that swarm: wrong codes are
   dropped silently, and no session ever forms (ADR-0017 §7). Rotating the
   code (restart `cairn signal --join-code <new>`) locks out every old
   holder. Smoke tests use `--dev-key`/`--swarm-dev-key` on both sides.
1. **Provision.** Create the tenant + admin:
   `cairn-server --data-dir /srv/cairn --grpc 0.0.0.0:7443 --objects 0.0.0.0:7444 --dev-insecure`
   (dev bootstrap; production disables `--dev-insecure` and issues codes via an admin
   device). Create the project: ctl `ProjectService::create_project`.
2. **Enroll devices.** Issue a single-use code (`Auth::enroll_code`, admin scope), then on
   each editor machine:
   ```
   cairn login --server studio-x.cairn.internal:7443 --code enr-... --name "edit-bay-2"
   cairn doctor            # must be HEALTHY before attaching roots
   ```
   Tokens live in the OS keychain (never plaintext; dev fallback is explicit-only).
3. **Attach roots.** `cairn daemon` runs; attach the project root via ctl
   `CtlProjects::attach_root`. Watch `cairn status` until the initial sync settles.
4. **NLE spot check (per SPEC §10).** Confirm NLE media caches point at local scratch;
   pin the current project file (`cairn pin --project p1 --path scene.prproj`); verify
   leases appear (`cairn lease ls`).
5. **Acceptance.** Two devices share one project file; save from both; one device receives
   the conflict copy path `"name (conflict — device — date).ext"`; journal cursors converge;
   `cairn doctor` stays healthy.

## Waveform service (review portal, CONTRACT-DEBT #4)

The portal computes scrub-waveform peaks SERVER-side (`GET
/r/:token/waveform/:version`): symphonia (pure Rust) streams the media
packet-by-packet into per-bin min/max — memory is one decode buffer plus
a bins array capped at 200k bins (~1.6 MiB), so peak RSS does not scale
with duration. The player falls back to its own browser decoder whenever
the endpoint refuses, so a misconfigured service degrades, never breaks.

Knobs (read once at portal construction):

| env | default | meaning |
|---|---|---|
| `CAIRN_WAVEFORM_LANES` | `2` | concurrent decode lanes; `0` disables the service |
| `CAIRN_WAVEFORM_RATE_HZ` | `8` | peaks per second of audio (bins density) |
| `CAIRN_WAVEFORM_MAX_MINUTES` | `240` | audio longer than this → `400 "audio too long for waveform"` |
| `CAIRN_WAVEFORM_TIMEOUT_SECS` | `120` | per-job decode timeout (→ `503`) |
| `CAIRN_WAVEFORM_RETRY_AFTER_SECS` | `2` | seconds advertised in `Retry-After` on 429 |

Failure contract (body is always `{"ok":false,"error":...}`):

- lanes full → `429`, error `RATE_LIMITED`, `Retry-After` header — the
  portal never queues a guest behind another decode; the browser decodes
  locally instead (budget 40 MiB, as before).
- `lanes = 0` → the endpoint answers `503 "waveform disabled"` without
  touching any file (the kill switch; no restart needed to shed load
  beyond it — it applies on portal restart).
- unsupported codec (only WAV/FLAC/MP3/OGG-Vorbis decode today) → `415`;
  overlong audio → `400`; decode timeout → `503`.

Cache: `<blobs_root>/waveforms/<blake3-of-media-bytes>.json` (or
`<tempdir>/cairn-waveforms` when no blob tree is reachable). Keys are
content-addressed, so entries survive re-publishes of the same bytes and
are safe to delete at any time; responses are served with
`Cache-Control: public, max-age=86400`.

Benchmark + memory evidence: `docs/BENCHMARKS.md` → "Waveform peaks
(server-side)"; repro `bash scripts/bench_waveform.sh`.

## Gates before a studio goes live
- [ ] `cairn doctor` healthy on every device
- [ ] canary loop green for 24h (`jobs` table / `cairn_canary_loop_result` metric)
- [ ] GC shadow report clean (`docs/runbooks/gc-shadow.md`)
- [ ] Kill-switch drill: flip `packing_enabled`/`tiering_enabled` off/on mid-traffic — no
      restart, no errors
- [ ] DR walkthrough: `docs/runbooks/dr.md` table-top
- [ ] Security sweep green: `just security` (RustSec, secrets, unsafe policy,
      path-containment, TLS fail-closed, I3, token-log, ctl scopes) — WO6-9
- [ ] NLE human-gate matrix executed on a studio Windows box:
      `docs/design/nle-test-matrix.md` (Premiere H1–H3, Resolve H4–H5,
      Blender H6–H8, conflict H9, offline H10)
- [ ] Bucket posture verified private (no anonymous List/Get) — see
      `docs/design/public-bucket-exposure-notes.md`; operator checklist in DR runbook

## Golden corpus ingest (§15.3)
Real NLE save sequences are LFS-gated. Per studio: collect 10+ auto-save sequences
(.prproj/.drp) + BRAW/ProRes/MXF/WAV samples into `corpus/<studio>-<seq>/NN.ext` (save
order), then `git lfs add corpus/**`. The corpus harness (`cairn-core` tests) gates on
chunk-reuse >70% per sequence.

## Support triage map
- Sync errors → `cairn status --json` + outbox depth (doctor)
- Hydration latency → `cairn_hydration_first_byte_ms` metric (I1 alert)
- Lease complaints → `docs/runbooks/lease-restart.md`
- Slow recalls → `docs/runbooks/recall.md`
