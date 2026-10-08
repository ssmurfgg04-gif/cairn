# Contract debt register — the "machinery exists, end-to-end wiring isn't finished" list

The architecture review's honest note: several same-contract goals are
*aspirational* — the mechanism exists, but the full wiring doesn't, and
the ADRs say so. Untracked aspirations rot; this register gives each one
an owner-shaped definition of done. Each item: what exists, what's
missing, the acceptance test that closes it.

## 1. Sync-engine conflict → automatic offer/application — **CLOSED this round**

- **Existed:** the C0–C10 classifier and three-way merge in `cairn-tl`;
  conflict-copy naming in the engine; `tl_merge` served on the console
  (`POST /api/v1/tl-merge`) and in the CLI (user-initiated only).
- **Closed:** the engine now OFFERS the merge. On a §7.1 CONFLICT for an
  `.otio`/`.fcpxml` path, after the conflict copy, the engine resolves the
  base (the `base_seq` ancestor) and winner manifests from the journal,
  fetches both via CAS, and runs `cairn_tl::merge_with` under the semantic
  policy — a machine-applicable result (Clean/Notes) is stored as a
  `merge_offers` row (store migration v5). `semantic_merge` (default OFF,
  ADR-0023) gates the offer; Conflicts/C10 outcomes still get NO offer —
  the manual tl-merge UI remains the path for those. The dashboard Files
  view gains a per-row "merge available" affordance with accept/decline
  (`POST /api/v1/merge/offer/accept|decline`); accept recomputes the
  deterministic merge, writes the canonical OTIO to the original path and
  appends exactly ONE journal upsert (the conflict copy is removed);
  decline keeps the copy and clears the offer.
- **Acceptance (met by tests):**
  `crates/cairn-sync/tests/merge_offer.rs` drives two real engines over
  one journal that emulates the §7.1 rule — flag-off leaves no offer,
  flag-on creates one (`stats.merge_offered`), decline keeps the copy,
  and accept writes one journal entry after which BOTH devices converge
  on the merged timeline (composed head+tail trim asserted).
  `crates/cairn-sim/tests/state_and_offer_e2e.rs::
  merge_offer_flow_through_real_server_e2e` repeats the flow against the
  real server journal.

## 2. Live-presence / join-time flag restart behavior — **CLOSED this round**

- **Existed:** `ensure_swarm` read `flag:live_presence` (and `fec_parity`,
  `quic_relay`) per swarm join, but nothing rejoined a live swarm after a
  flip — the honest ADR-0023 note said "applies at next attach/daemon
  start", which users read as a broken toggle.
- **Closed:** `ProjectManager::restart_swarms` (cairn-cli/src/projects.rs)
  shuts down every live swarm and rejoins with freshly-read flags the
  moment the ctl `set_flag` commits one of the three join-time flags.
- **Acceptance (met):** flip `live_presence` in the dashboard with the
  daemon running and a swarm joined → the SSE `/api/v1/live` stream goes
  live and `GET /api/v1/live/snapshot` reports `enabled:true` within a
  second, no re-attach, no daemon restart. Flipping OFF likewise drops
  presence without a restart.

## 3. Real Premiere-host verification for the UXP panel

- **Exists:** the marker bridge contract (ADR-0022 follow-up): CLI export
  and the `/api/v1/markers` endpoint share `handoff::markers_payload` —
  one builder, two of the three surfaces; the panel ships with the repo.
- **Missing:** the third surface — a real Premiere host driving the panel
  end-to-end — has never been verified on-VM; the nle-matrix results are
  DaVinci + CLI + HTTP curl.
- **Acceptance:** the nle-matrix workflow gains a Premiere-hosted run
  (UXP runtime available in the CI image) whose transcript shows the
  panel fetching markers for the named project and receiving 200 with a
  parseable FCPXML; matrix cell turns green in `docs/nle-matrix-results`.
- **CI scaffold added 2026-10-08; real host run still pending** —
  `premiere-uxp` job in `.github/workflows/nle-matrix.yml`
  (dispatch-only, non-blocking, `[self-hosted, windows, premiere]` label
  unclaimed) + `--host premiere` in `scripts/nle_matrix_collect.py`;
  setup and the green checklist: `docs/nle-matrix-results/premiere-uxp-README.md`.

## 4. Server-side large-waveform handling — **landed this round (gate + bound); host-rerun of the bench is routine**

- **Existed:** per-chunk streaming upload/download; client-side-only
  peaks in the portal (`review.js`, 40 MiB `WAVE_BUDGET`); proxy
  fast-path (round 29 keepalive). NOTE: this register previously cited
  "ADR-0028 §B generates server-side" — that citation was WRONG
  (ADR-0028 §B is note Ranges); no server waveform code existed.
- **Closed:** server-side peaks now exist —
  `GET /r/:token/waveform/:version` in the portal
  (`cairn-review::waveform::WaveformService`): streaming symphonia
  decode (constant decode buffer + a capped bins array — peak RSS
  measured at ~1–3 MiB for a 130-minute bed), disk-cached by content
  hash, admission-gated by a lane semaphore → `429` + `Retry-After`
  (`RATE_LIMITED`, documented in ctl-api per ADR-0010 lockstep), plus
  `CAIRN_WAVEFORM_LANES / _RATE_HZ / _MAX_MINUTES / _TIMEOUT_SECS`
  knobs (runbook). The portal player prefers the server peaks and falls
  back to its own decoder on any non-200.
- **Acceptance (met):** `docs/BENCHMARKS.md` "Waveform peaks
  (server-side)" records the measured 130-minute bed: 5.7 s wall,
  2.8 MiB peak RSS delta (constant, not proportional to duration),
  429 + Retry-After when lanes are exhausted. Remaining (routine):
  re-run `scripts/bench_waveform.sh` on a production-class host and
  paste the row.

## 5. Synced collaborative state (roster / review / audit across devices) — **CLOSED: Phases 1–3 all landed (round 31)**

- **Existed:** the durable bindings, the engine's keyed append-only
  journal with tombstone semantics, and per-machine members/audit/review
  files (machine-local).
- **Phase 1 (landed):** the `state_record` journal op family
  (PROTO_VERSION 4 → 5) carries five record families — member
  (LWW per device_id, tombstone wins ties), audit (append-only union by
  the audit content id), review_version (append-only), review_link /
  review_comment (LWW per key; a revoke is a tombstone). Records ride
  the existing WAL/outbox/convergence machinery (durable-before-send,
  kill -9 safe); the server journal applies NO conflict rule to them and
  compaction exempts `state/%` paths (cold attach replays them);
  member/audit/review-publish/link/revoke daemon surfaces publish
  fire-and-forget; the dashboard Team view shows the synced roster
  beside the machine-local one (`GET /api/v1/state-records`), and the
  local members.json remains the enforcement authority until Phase 3.
- **Missing (Phase 2):** the tombstone-backed revoke EVERYWHERE — the
  portal RootProvider gains a `link_revocations(project)` lookup
  consulted before minting/validating, so a revoke issued on any
  attached machine is dead on every portal within one sync pass (the
  tombstone records already flow — the wiring is the remaining work).
  Phase 3: `.cairn` becomes a materialized view of the records.
- **Phase 2 (LANDED round 31, Task 2-a):** the RootProvider
  `link_revocations(project)` lookup now exists and is consulted before
  minting/validating — the revoke-anywhere-dead-everywhere behavior holds
  within one sync pass.
- **Phase 3 (LANDED round 31, main thread):** `.cairn` is now a
  MATERIALIZED VIEW of the records (`cairn-cli/src/materialize.rs`, wired
  via the `StateMaterializer` engine seam): after every sync pass that
  applies state records, members.json follows member records (upsert per
  device, tombstone removes), audit.jsonl unions by content id, and
  review.json follows review_version/review_link/review_comment records —
  so a cross-machine revoke removes the link from the FILE every local
  reader (shell ext, CLI, offline machines) actually opens. LWW echo
  guard (an apply-echo older than the file's own entry never regresses
  it), corrupt cache files reported-never-clobbered, materializer failure
  never fails the sync pass (records remain truth; the cache is
  best-effort). rbac_guard untouched — it reads members.json, which now
  follows the records; offline machines keep local authority until
  records arrive (the honest boundary). Proxy caches stay outside the
  record set permanently, per the ADR.
- **Acceptance (met):** the three legs are covered — same roster as
  authority (materialization + synced records, sim E2E), comments cross
  devices (review_comment records + materialization, unit + sim), and a
  link revoked on either machine is dead on both portals within one sync
  pass (Phase 2 portal consult, `cross_machine_revoke_kills_the_link_on_
  the_other_portal_e2e`) AND out of the local file everywhere (Phase 3
  materialization, `review_link` tombstone unit + sim tests).
- **Acceptance (Phase 1, met by tests):**
  `crates/cairn-sync/tests/state_records.rs` (two engines, one journal:
  LWW convergence, audit union, tombstone propagation) and
  `crates/cairn-sim/tests/state_and_offer_e2e.rs` (roster + audit-union
  + link-revoke over the REAL server; crash-resume durability for a
  pending record). Phase 2's acceptance stays as written below.
- **Acceptance (Phase 2):** two attached devices see the
  same roster in the dashboard Team view as authority (not just
  visibility), a comment left on device A appears in device B's review
  view, and a link revoked on either machine is dead on both portals
  within one sync pass.

---

Ordering update (2026-10-08): #1 CLOSED (engine-offered merge behind
`semantic_merge`), #4 landed (admission-gated server peaks + measured
bench), #5 Phase 1 landed (`state_record` op family). #2 was closed last
round. Superseded by the round-31 updates below — #5 is now CLOSED
(Phases 1–3). Remaining open: #3 (a real Premiere host run on a
self-hosted runner; the CI scaffold and collector are ready).

---

Ordering update (2026-10-08, round 31) — what moved, honestly:

- **#5 Phase 2 (cross-machine revoke): LANDED** (Task 2-a): the portal
  consults the synced `review_link` tombstones before minting/validating,
  so a revoke on any attached machine kills the link on every portal
  within one sync pass. The register's phase-2 acceptance (roster as
  Team-view authority, cross-device comments, cross-machine revoke) is
  the test surface 2-a ships against. Phase 3 (`.cairn` as a materialized
  view of the records) remains the last phase.
- **Review-link TTL: LANDED** at the route + UI layer (Task 2-a plumb,
  Task 2-b chooser): `POST /api/v1/review/link` takes optional
  `ttl_hours` (1..=8760, default 720).
- **Portal rate limiting: LANDED** (Task 2-a) on the anonymous guest
  surfaces — closes the second-review "#57–#60… review-endpoint rate
  limiting" item's core (review-side; the full #65–#72
  cargo-deny/SBOM/provenance/signing cluster stays open — see the
  installer row below for the supply-chain-light slice that landed).
- **Join codes: LANDED LOCALLY, SERVER-SIDE STILL TRACKED** (Task 2-a):
  the swarm join code (ADR-0017) gained single-use burn / revocation /
  lockout on the hosting side. The SERVER-authoritative enroll-code
  protocol — admin-scoped minting on a clean server, expiring+revocable
  server codes, explicit fail-closed default for unknown devices —
  REMAINS OPEN (the meeting-point runbook documents the current
  dev-insecure-window bootstrap and its limit).
- **Proxy editing copies: LANDED** end to end (Task 2-d service + routes
  per the frozen contract, Task 2-b UI), so the ADR-0020 proxy story is
  wired: generate/status/state surfaces + ffmpeg-backed transcode.
- **ffmpeg/ffprobe/merge off the async runtime: LANDED** (Task 2-d) —
  closes the substance of "#45–#51: move ffmpeg/ffprobe out of the async
  runtime" (bundling-or-dropping media tooling remains a product call).
- **#41–#44 cluster: LANDED** (Task 2-d): duplicate-collision guard made
  atomic (single transaction), restore takes a safety checkpoint, confirm
  hardening. Idempotent-mutation + disable-during-mutation finishes stay
  with the 2-b UI pass at merge.
- **Two-home cross-network E2E: LANDED** (Task 2-d,
  `crates/cairn-sim/tests/two_homes_e2e.rs`) — amber until a real
  two-homes HUMAN test (BETA.md §8); **real-media editing-session E2E:
  LANDED** in CI, amber until the Premiere host run (#3 above).
- **NSIS installer: LANDED** (Task 2-c): `installer/windows/cairn.nsi` +
  `.github/workflows/installer.yml` with a silent-install gate on a clean
  runner and SHA256 + build metadata in the job summary — the
  supply-chain-LIGHT slice of #65–#72. Signing (Authenticode, review-item
  #72) stays open: the installer is unsigned today.
- **Meeting-point runbook: LANDED** (Task 2-c):
  `docs/runbook-meeting-point.md` + hardened systemd units, Dockerfile,
  compose, nginx — including the honest admission gap (no admin-scoped
  minting on a clean server; the bootstrap workaround is documented).
- **Plain-language pass: LANDED** (Task 2-b) — closes the wording half of
  "#73–#78 (user-facing error language, i18n audit, a11y finishes)".
- **#3 Premiere host run: STILL OPEN, AMBER** — nothing in this round
  changes its gate: it needs a real self-hosted Premiere run
  (`[self-hosted, windows, premiere]` label unclaimed).

---

## From the second product review (2026-10-08 paste) — accepted, tracked

The full paste lives at the repo root (`pasted contnet by ai reviewer`).
What this round already closed from it: **#1** cross-device state Phase 1
(= register #5 above), **#90** contract-debt #1 (closed above), **#91**
Premiere scaffold (#3), **#92** waveform admission control (#4), **#2**
RBAC holes — team_regenerate / review_publish / review_link /
review_revoke now funnel through `rbac_guard` at the service boundary
(actions tagged "POLICY BOUNDARY (ADR-0030)"), **#3** honest join copy
("Code saved — not joined yet" + the real next step, no more "Joined!"),
**#62/#63/#64** index.html is `no-store` with a self-hosted CSP,
nosniff + no-referrer, and the Google Fonts beacon is gone (system
stacks carry until fonts are vendored), **#6** symlink containment
proven against REAL links (in-root→outside file, outside dir,
symlink chain, in-root alias stays legal) — which surfaced and fixed a
real escape: a dangling leaf over an outside-pointing intermediate
symlink used to be accepted; safe_join now canonicalizes the deepest
existing ancestor before falling back.

Tracked next (not started, ordered by the review's own P0-first logic):

- **#4/#5 (P0):** join codes → single-use, expiring, revocable (today:
  TTL only, no single-use burn, no revocation list); unknown-device
  fail-open `Editor` → make the default explicit and configurable
  (fail-closed pending state) — more urgent now that rosters sync.
- **#7–#12 (P0/P1):** eliminate `first()`-style fallbacks for mutations;
  project context + project-scoped Team/review/markers/link surfaces.
- **#41–#44 (P1):** duplicate-collision guard, idempotent mutations,
  disable-during-mutation buttons, restore confirmation hardening.
- **#45–#51 (P1):** move ffmpeg/ffprobe out of the async runtime;
  bundle-or-drop media tooling.
- **#52–#56 (P1):** polling cost (visibility-aware expensive views),
  telemetry caching, fetch timeouts.
- **#57–#60, #61, #65–#72 (P1):** desktop lifecycle, token-in-URL
  reduction, secret-log audit, review-endpoint rate limiting,
  cargo-deny/SBOM/provenance/signing.
- **#73–#78 (P1):** user-facing error language, a11y finishes, i18n
  audit; **#79+ (P2)** polish, storage language, versions/restore
  wording, diagnostics/correlation IDs.
