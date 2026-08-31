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
