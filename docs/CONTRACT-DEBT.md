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

## 5. Synced collaborative state (roster / review / audit across devices) — **Phase 1 landed this round; Phase 2 (cross-machine revoke) is the next rock**

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
bench), #5 Phase 1 landed (`state_record` op family; enforcement still
machine-local until Phase 3). #2 was closed last round. Remaining: #5
Phase 2 (tombstone-backed revoke everywhere — the security payoff) and
#3 (a real Premiere host run on a self-hosted runner; the CI scaffold
and collector are ready).
