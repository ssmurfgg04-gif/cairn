# Beta runbook — onboarding 5 studios (M8)

Prerequisites read: SPEC.md, docs/ctl-api.md, docs/runbooks/*.

## Per-studio onboarding (CLI end-to-end)

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
