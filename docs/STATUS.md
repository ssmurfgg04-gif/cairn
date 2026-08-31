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
