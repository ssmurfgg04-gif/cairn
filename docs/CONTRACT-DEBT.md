# Contract debt register — the "machinery exists, end-to-end wiring isn't finished" list

The architecture review's honest note: several same-contract goals are
*aspirational* — the mechanism exists, but the full wiring doesn't, and
the ADRs say so. Untracked aspirations rot; this register gives each one
an owner-shaped definition of done. Each item: what exists, what's
missing, the acceptance test that closes it.

## 1. Sync-engine conflict → automatic offer/application

- **Exists:** the C0–C10 classifier and three-way merge in `cairn-tl`;
  conflict-copy naming in the engine; `tl_merge` served on the console
  (`POST /api/v1/tl-merge`) and in the CLI.
- **Missing:** the engine does not *offer* a semantic merge when a
  timeline conflict is detected during sync, and no surface applies the
  merged result back (the merge today is user-initiated).
- **Acceptance:** attach two devices, cut disjoint ends of one timeline
  on both, let sync converge → the dashboard Files view shows a
  "merge available" affordance sourced from the engine (not a UI
  heuristic); accepting it writes one journal entry and both devices
  converge on the merged timeline; declining leaves the conflict copy.
  Flag `semantic_merge` (default OFF, ADR-0023) gates the offer.

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

## 4. Server-side large-waveform handling

- **Exists:** per-chunk streaming upload/download; the waveform path
  (ADR-0028 §B) generates server-side; proxy fast-path (round 29 keepalive).
- **Missing:** waveform generation for a 90-minute multicam audio bed has
  no measured memory budget or admission gate — the plan said "server
  handles large files gracefully" and the current answer is "it handles
  them until it doesn't".
- **Acceptance:** a benchmark in `docs/BENCHMARKS.md` with a ≥2 h audio
  bed shows peak RSS bounded (constant, not proportional to duration)
  and a 429-with-retry-after when the admission queue is full; the
  runbook documents the knob.

## 5. Synced collaborative state (roster / review / audit across devices)

- **Exists:** the durable bindings, the engine's keyed append-only
  journal with tombstone semantics, and per-machine members/audit/review
  files (machine-local).
- **Missing:** the record sync itself — the whole of ADR-0031's Phases
  1–3. Until then, team state is per-machine and review comments don't
  travel (the markers bridge is the one cross-machine read path).
- **Acceptance:** ADR-0031 Phase 2 ships → two attached devices see the
  same roster in the dashboard Team view, a comment left on device A
  appears in device B's review view, and a link revoked on either machine
  is dead on both portals within one sync pass.

---

Ordering: #2 is done (this round, cheapest + a UX lie today). #5's Phase 1
is the next big rock (security payoff: cross-device revoke). #3 and #4 are
verification/budget work gated on CI resources. #1 is the only one that
adds engine surface — deliberately last, behind its flag.
