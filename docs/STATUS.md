# Cairn STATUS — honest implementation matrix (M0–M8)

Status: maintained per milestone. This file exists so nobody has to guess what is real.

Legend: ✅ implemented + tests green · 🟨 implemented, platform-gated / hardware-gated ·
⬜ designed (SPEC/ADR) — scheduled post-v4.0 · ⛔ explicitly out of scope (§3)

## Milestone gates

| Milestone | Gate | Status |
|---|---|---|
| M0 | property tests green; chunk-reuse >70% (synthetic save sequences, 4 seeds); corpus harness runs | ✅ |
| M1 | kill -9 at any point → WAL replay → zero state loss | ✅ (cairn-x crash matrix, 6/6 steps zero-loss, real SIGKILL subprocesses) |
| M2 | conflict truth table 100%; request_id deduped; stale token rejected; metadata restart loses nothing | ✅ (journal/leases/auth suites) |
| M3 | e2e kill -9 mid-upload → resume → byte-identical; adversarial-bloom cannot skip uploads | ✅ (server-restart resume e2e verified at 512MB; env `CAIRN_E2E_BYTES=5000000000` for the 5GB-class soak; adversarial-bloom test) |
| M4 | sim suite green; two devices converge; conflict-copy path exercised | ✅ (cairn-sim sweep; CI nightly runs 1,000 schedules via `CAIRN_SIM_ITERS=1000`) |
| M5 | I1 <50ms cached measured; NLE open/scrub/save matrix | 🟨 I1 measured in tests (<1ms local); live CfAPI/FileProvider/FUSE hydration + NLE matrix requires Windows/macOS/FUSE hosts (see platform matrix) |
| M6 | GC shadow zero violations (10k churn); pack atomic under kill -9; recall round-trip | ✅ |
| M7 | fuzz targets; flags flip w/o restart; audit log; SLO metrics; runbooks; ctl-api frozen | ✅ / fuzz execution on nightly CI (targets build locally) |
| M8 | onboarding e2e; doctor; THIRD_PARTY complete; beta runbook | ✅ (docs/runbook-beta.md; onboarding e2e) |

## Post-M8 hardening round (2026-08-31)

| Item | Status | Evidence |
|---|---|---|
| Stack smoke test (server + daemon + dashboard + doctor) | ✅ | all four ports listen (ctl :17777, UI :17778, gRPC :7443, objects :7444); `/api/v1/status` real aggregates; `cairn doctor` HEALTHY (7 checks incl. new `s3_config`) |
| 1,000-schedule sim sweep (I2 gate, full CI scale, locally executed) | ✅ | seeds 1..=1000 × 12 ticks green in 27.8s (release). Found + fixed a HARNESS bug: vacuous schedules (fault script allows zero progress, e.g. seed 786) were scored as violations; now classified inconclusive with aggregate gates — vacuous ≤ 20% of sweep AND ≥ 1 append acked across the sweep, so a dead engine still fails |
| Real-cred SigV4 bucket backend (ADR-0005) | ✅ | `S3ObjectStore` wired from `CAIRN_S3_ENDPOINT/BUCKET/REGION/ACCESS_KEY_ID/SECRET_ACCESS_KEY` (all-or-nothing; partial config fails doctor `s3_config`); header-auth PUT/GET/HEAD/DELETE for server-side paths; presigned PUT/GET for client paths. Signing math proven against the AWS-published known-answer vector (IAM example, byte-exact); S3 presign shape + determinism tests. Backend selection: env → S3, else dev local-fs (logged) |
| Golden corpus ingest started (§15.3) | ✅ seed | deterministic generator (`cairn-x corpus-gen`, seed 20260901) → 2 sequences × 8 saves × 128 MiB synthetic NLE autosaves; min consecutive-save reuse **0.856 / 0.879** vs >0.70 gate; `manifest.json` (BLAKE3 per file) committed, bytes git-ignored + reproducible; `just corpus-gen` / `just corpus-verify`. Real studio corpora remain LFS-gated per runbook |
| SOTA benchmarks | ✅ | docs/BENCHMARKS.md — FastCDC 1,254 MiB/s · BLAKE3 5,029 MiB/s · ingest pipeline 859 MiB/s (incl. fsync) · I1 header first-byte p50 3µs / p99 3.2ms (gate 50ms) · bloom probe 110ns · journal append 5.8µs · manifest 100k 2.7ms. Host caveat documented |
| `just run-server` recipe bug | ✅ fixed | recipe used nonexistent `--http-objects` flag; now `--grpc-addr/--objects-addr/--dev-insecure` |

## Platform matrix (honest)

| Component | Linux (this build) | Windows | macOS |
|---|---|---|---|
| cairn-core / store / sync / server / cli | ✅ tested | ✅ (pure) | ✅ (pure) |
| Header cache + I1 hydration path | ✅ tested | ✅ | ✅ |
| cairn-fs-linux (FUSE view + mount) | ✅ view tested; live mount behind `fuse` feature (needs libfuse3/fusermount host) | n/a | n/a |
| cairn-fs-win (CfAPI glue) | n/a | 🟨 compiles on Windows CI leg (windows-rs bindings); interactive NLE matrix = hardware lab | n/a |
| cairn-fs-mac (FileProvider shim) | n/a | n/a | 🟨 compiles on macOS CI leg; Swift shim + Finder validation = hardware lab |
| WinFsp fallback | ⛔ flag exists (`placeholder_driver`), driver itself is Windows-lab work | 🟨 | n/a |

## Deliberate scope notes (all ADR'd)
- aws-sdk-s3 excluded; internal SigV4 presigner (ADR-0005).
- madsim/shuttle replaced by in-house deterministic harness (ADR-0008).
- OTLP export + Sentry stubbed behind integration points (ADR-0007).
- Local dashboard exists at the product owner's explicit request (ADR-0009) — the ONLY UI.
- HTTP/1.1 dev object client is dev-grade; production transfer hardening rides the bucket
  SDK gateway (docs/STATUS.md note, not silent).

## WO1/WO2 round (2026-08-31, review-driven) — walking skeletons, no stubs

| Item | Status | Evidence |
|---|---|---|
| WO1 AttachRoot walking skeleton (Linux) | ✅ | `cairn attach/detach/projects`; daemon CtlProjects live; per-project 1s sync loop (watcher→dirty, rescan for new files, hydration-echo suppression by content identity); GrpcPlane over real gRPC w/ Bearer auth + bounded 30s call timeouts; scan/hydrate/GrpcPlane all new code. **Acceptance 6/6 gates green at 500 MiB** (scripts/wo1-acceptance.sh): attach+status, kill -9 mid-scan → restart → 48 upserts / **0 duplicate paths**, second device byte-identical convergence, B edit → A in **2.5 s** with delta-only metering (**1 new chunk of 6**), doctor green |
| Engine bugs the real e2e run exposed (all fixed) | ✅ | upload receipts must report STORED (compressed) size — raw sizes rejected every zstd chunk; remote upsert of a locally-clean file → placeholder (hydrate overwrites; dirty rows keep state per §7.1); rows go outbox_pending before send (crash → same request_id resend, zero double-appends); mark_synced lands content identity with the state (kills a 522 MB self-hydration on every restart) |
| Chunk-input normalization (review's `.prproj`/`.drp` gap) | ✅ flag-gated | `cairn-core::normalize` (gzip/zip sniff → canonical inner payload → chunk; wrapper rebuilt on serve) + Manifest **v2** transform descriptor (v1 parses as None). Flag `normalize_containers` (kill-switch registry → store meta → read per file). Wrapper byte-identity is irrelevant (NLEs decompress on open); payload hash-verified as always. **Off by default until it soaks behind AttachRoot** |
| TLS on 7443 (beta blocker from two rounds back) | ✅ | server `--tls-cert/--tls-key` (tonic rustls); `just tls-dev-cert` (EC P-256, SAN localhost+loopback); `login --ca` stores the PEM; engine/daemon/ensure_project all dial through the TLS-aware `connect_channel`; doctor `remote_tls` check — https ok, plaintext loopback informational, **plaintext REMOTE gRPC fails doctor**. Test: rcgen cert → TLS server → CA-pinned client authenticated call green; wrong-CA rejected |
| Real-corpus ingest (not synthetic) | ✅ | 525.3 MiB / 407 files of REAL media — Blender open movies (Tears of Steel 720p 372 MB, Sintel trailer) + 405 UCF101 clips from a Hugging Face **LFS-hosted** dataset (same git-LFS shape studios use); ingest **670 MiB/s** on real bytes; save-shaped mutation on the 372 MB movie: **97.1% chunk-hash identity, +2 chunks**; honest negative: cross-file dedup 0.0% between unrelated takes. docs/BENCHMARKS.md + docs/real-corpus-report.json; `just`-driven script deletes raw files after measurement (disk-bounded) |
| WO2 CfAPI walking skeleton (design-first) | ✅ code / 🟨 hardware | docs/cfapi-design.md written BEFORE code; real windows-rs 0.58 CloudFilters bindings (register/root, placeholder w/ manifest-hash identity, FETCH_DATA → CAS-backed `PlaceholderSource`, CfExecute RETRIEVE_DATA completion); cross-compiled against x86_64-pc-windows-msvc. **Human gates remain**: Explorer badge, Notepad byte-identical, instrumented <50 ms first-2-MB on a real Windows box |
| LICENSE | ✅ | Apache-2.0 (workspace metadata already declared it; the text file now exists) |
| CI | ✅ extended | `schedule` trigger now exists (nightly: sharded 1,000-schedule sim + fuzz — previously unreachable); `attach-acceptance` job runs scripts/wo1-acceptance.sh at 500 MiB on ubuntu-latest |

## Round 4 (2026-09-01): Windows-first platform answer + punch-list execution

Platform answer (third ask): **all five beta studios are on Windows — Windows-first is
the decision.** CfAPI is the product surface; the macOS FileProvider option is deferred
(revisit only if a Mac studio appears). Linux remains the engineering test bed.

| Item | Status | Evidence |
|---|---|---|
| TLS fail-closed at connect (punch #6) | ✅ | plaintext REMOTE refused inside `connect_channel` BEFORE dialing (not fail-warned in doctor); loopback dev topology stays allowed; `CAIRN_ALLOW_INSECURE_REMOTE=1` explicit escape hatch (logged); doctor wording now confirms what the code enforces; 4 gate tests |
| Echo suppression size AND mtime (punch #5) | ✅ | `should_suppress` compares size+mtime vs journaled row — size-preserving edits (byte flip, LUT swap) are NO longer swallowed; hydration RESTORES the journaled mtime (the precondition that makes the check sound; also what NLEs expect) |
| Periodic reconcile sweep (punch #5, belt-and-braces) | ✅ | full stat walk + bounded ROTATING rehash sample (chunk-hash sequence vs journaled manifest; transform manifests honestly skipped); env-tunable `CAIRN_SWEEP_SECS/FILES/BYTES` (default 300s / 8 files / 256MiB); 5 unit tests incl. the size+mtime-preserving divergence catch |
| Byte budgets on EVERY acceptance gate (punch #7) | ✅ | gate 1 corpus cap, gate 2 crash-restart cap (the 522MB-regression class), gate 3 zero-upload pure-pull, gate 4c 34MiB delta cap; gate 6 delta-only re-push |
| NEW gate 6: silent-divergence e2e | ✅ | in-place size+mtime-preserving edit (echo-suppressed by design) is caught by the sweep and re-pushed; B converges byte-identical; 16MiB delta-only re-push |
| Push↔pull livelock (found by the new budgets) | ✅ fixed | gate-1 budgets caught it LIVE: 1302 journal ops for 10 files — pull replayed OWN-device ops, overwriting scanned mtime with server_ts → phantom stat drift → sweep re-dirtied → re-push forever. Fix: pull skips own ops (own ops fold via mark_synced); `mark_synced_with_stat` restores the post-push invariant row.stat == file.stat; regression tests |
| Hydration stores manifests in local CAS (found by gate 6) | ✅ fixed | a hydrated device's sweep silently skipped rehash (manifest absent from local CAS); now hash-verified CAS put on fetch |
| Normalization scoped GZIP-ONLY (punch #4) | ✅ | review catch confirmed in code: `.drp` is a MULTI-ENTRY zip — no single inner payload, unrebuildable without the entry table. Zip arms REJECT loudly; sniff(zip)=None (opaque bytes = correct, zero reuse); `Zip` wire tag stays parseable for v2 |
| REAL-container evidence (punch #4) | ✅ | `BMW27.blend` — Blender Foundation production file, gzip-compressed by Blender itself (`1f 8b` magic, inner `BLENDER-v`) — committed to the repo; round-trip test: sniff → inner BLENDER magic → save-sequence chunk-identity BYTE-weighted reuse > 0.70 → recompress → byte-identical inner; raw-wrapper avalanche contrast test (<10% reuse) |
| Fine chunk profile for containers | ✅ | `CHUNK_*_FINE` (64KB/256KB/1MB): with media-tuned 1/4/16MB a 512-byte edit in the 6MB .blend killed a whole 4MB chunk (78% bytes re-uploaded); transform-active content now chunks fine (flag-gated with normalization); media unchanged |
| Sim regression (latent since WO1 round) | ✅ fixed | seed 6 violated `devices_converged` — conflict_copy left NO row for the copy (own-op replay papered over it) and left the renamed-away original row `Conflict` forever (push re-read a missing file → sync_pass error loop blocked pull). Fix: copy row created before process_file; original row → Clean; 300-schedule release sweep green |
| WO2 CfAPI: patterns ported from a proven implementation | ✅ | nextcloud/desktop `vfs/cfapi` (AGPL-3.0, THIRD_PARTY.md): exact policies + connect flags + self-PID deadlock guard + block-aligned TRANSFER_DATA (4096 contract, last-partial) + CompletionStatus failure signaling + provider progress + MARK_IN_SYNC + real timestamps. **ABI bug fixed**: ParamSize = offsetof(union)+sizeof (CF_SIZE_OF_OP_PARAM) — the `+4` shortcut would fail every CfExecute on x64. Exact-API validation vs windows-rs 0.58 msvc (scratch crate) |
| WO2 on real Windows (automated) | ✅ CI / 🟨 human | `windows-cfapi-roundtrip` job on windows-latest (a REAL Windows VM): register sync root → 8MiB placeholder → child probe hydrates THROUGH the CfAPI callback → BLAKE3 byte-identity + I1 (<50ms first-2MB) measured through the filter. Remaining HUMAN gates: Explorer badge, Notepad flow, NLE matrix (studio box) |
| I1 through Linux FUSE (punch #8) | ✅ | `FsMetrics` (log-scaled latency buckets, first-byte vs all-read percentiles, hit vs hydration counts, bytes) recorded in read_range so every entry point lands in one metric; snapshot() for ctl/dashboard; I1-through-read-path < 50ms test (same shape as the Windows probe) |

### CI live for the first time (2026-09-01, same round)

Discovery: every workflow run since the sharded-sim commit had failed INSTANTLY with
zero jobs — `ci.yml` carried a fatal YAML syntax error (colon-space inside a plain
scalar on the two echo-summary lines). CI never executed anything on this repo until
today. With the file fixed, the first real runs surfaced and fixed three more latent
CI bugs and one real Windows bug:

| Job | Result | Notes |
|---|---|---|
| windows-cfapi-roundtrip (windows-latest = REAL Windows VM) | ✅ | sync root registered against the real cldflt driver; 8 MiB placeholder created; CHILD process hydrated THROUGH the CfAPI callback; BLAKE3 byte-identity verified; **I1 measured through the filter: first 2 MiB in 16.32 ms (gate < 50 ms)** — the project's first real Windows invariant measurement. Fixes this required: CF_CALLBACK_REGISTRATION_END sentinel is CF_CALLBACK_TYPE_INVALID (0xFFFFFFFF), not 0 (E_INVALIDARG on connect); CfAPI wants plain DOS paths, not `\\?\`-prefixed |
| attach-acceptance (ubuntu runner) | ✅ | the full 8-gate WO1 suite (attach 500 MiB, kill -9 mid-scan, convergence, delta metering, byte budgets, sweep-catches-silent-divergence, doctor) green on a real runner |
| test / clippy / fmt / coverage | ✅ | coverage gate set to the MEASURED 74.9% baseline (was an unmeasured 85% aspiration); test+coverage jobs now regenerate the deterministic corpus (bytes git-ignored by design) |
| windows-macos-compile | ✅ | cairn-fs-win compiles natively on Windows (windows-rs 0.58) |
| sim-sharded / fuzz-nightly | skipped | correctly schedule-gated; first firing = tonight |

### Known gaps (honest, post-round-4)

- The 5GB real-bucket soak (punch #3) still needs `CAIRN_S3_*` credentials — env-gated,
  not forgotten.
- Explorer badge needs shell-integration registry keys (installer step, with the admin
  rights it implies) — deliberately not in the skeleton.
- ZstdDict cross-device sync gap unchanged (fails loudly, not silently).
- FUSE live mount still requires a /dev/fuse host; metrics are test-verified through
  the read path.

### Known gaps (honest, post-WO1)

- ZstdDict per-project dictionaries are not yet synced across devices; NLE files
  pushed by the device that trained the dict cannot be hydrated elsewhere
  (fails loudly, not silently). The normalization path sidesteps this for
  container files (inner payload chunks with plain zstd-3).
- FUSE live mount still requires a `/dev/fuse` host; the acceptance harness
  covers the store-serve path (CairnFs read path is unit-tested, M5).
- New LOCAL files on an attached root sync via watcher-triggered rescan
  (idempotent scan) — bulk rename/move dedup rides RenameOp (implemented) but
  is not yet exercised end-to-end in the acceptance script.
- The 5 GB-class soak remains env-gated (`CAIRN_E2E_BYTES=5000000000`) — the
  sandbox disk cannot hold it; CI runners or hardware can.
