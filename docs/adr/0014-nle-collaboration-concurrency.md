# ADR-0014: NLE collaboration concurrency — offload, partition, heartbeat, merge

Date: 2026-09-02
Status: Accepted (Phases 1, 2 and 3 implemented; Phase 4 is v2)
Supersedes: none
Related: SPEC §8 (leases & fencing), docs/design/write-back.md, ADR-0004

## Context

Teams do not want a manual pen. The pre-ADR lease model (60s TTL, acquired on
project-file open, no heartbeat, no reaper, no release-on-close) had three failure
modes that all landed on a human:

1. **Abandoned pens.** An editor crashes (or force-quits) holding a lease on
   `scene.prproj`. The lease lingers up to 60s, then expires — but peers wait, and
   an impatient operator runs `cairn lease release` BY HAND. Every crash becomes a
   support ticket.
2. **Starved pens.** Conversely, a legitimately open 45-minute editing session
   outlives the 60s TTL; renewal "rode the open/close cycle", which no NLE does.
   The save-back then fails with STALE_LEASE → conflict copy, at the worst moment.
3. **Fought-over pens.** Where a vendor ALREADY arbitrates (Premiere Productions'
   `.prodsys` engine, Resolve's PostgreSQL collab), a second arbitration layer on
   top is not defense-in-depth — it is two pens trying to write one file.

The strategic frame (review round, 2026-09-02): media assets — 99.9% of bytes — are
immutable and content-addressed, so they are ALREADY lock-free concurrent (Cairn CAS
gives this for free). The entire collision surface is the 0.1% of bytes that are
mutable project state. Do not solve at the byte level what can be eliminated at the
structural level.

## Decision

Four phases, in this order:

### Phase 1 — Native collaboration passthrough (SHIPPED)

`cairn_sync::native_collab` detects vendor-native multi-user modes and Cairn STANDS
DOWN: no lease is acquired, no fencing is imposed, arbitration is delegated to the
vendor engine.

Detected today:
- **Premiere Productions**: any `.prodsys` directory component in the path, or any
  `.prodsys` directory beside an ancestor of the file (the production DB dir; the
  `.prproj` files inside are vendor-managed pointers).
- **Operator-declared**: a `.cairn-native-collab` marker in the workspace root whose
  content is a mode line (`resolve-collab`, `production`, `custom`). Resolve's
  PostgreSQL collab has NO portable on-disk marker in project files — we do not
  pretend to sniff it; operators declare it. Honesty over magic.

We deliberately do NOT parse project-file schemas to detect collaboration modes.
Schema drift between NLE point releases is exactly the fragility Phase 4 rejects.

### Phase 2 — Domain decomposition (SHIPPED: config-enforced per-subproject scoping)

The highest-yield structural fix costs zero bytes of merge logic: scaffold projects
into sub-project scopes (`Reel_01.prproj`, `Audio_Conform.prproj`,
`VFX_Imports.prproj`) and let leases enforce single-writer per SUB-project. Two
editors on two reels never collide because their state boundaries do not overlap —
the lock surface shrinks by the decomposition, not by smarter locking.

**Now enforced by config, not team discipline:** a project MAY ship a
`.cairn-domains` file in its synced project root (one subproject root per line — an
ordinary synced file, so config propagates to every device through the sync engine
with NO wire or server change; every client resolves the identical scope
deterministically). A write-open under a declared root takes its lease at the
DOMAIN scope (`cairn_sync::domains`): a second file in the same domain hits the
live foreign pen (EBUSY), while other domains and unscoped files proceed
per-file (Phase 3 semantics unchanged). Parsing is lenient (bad lines skipped,
missing file = per-file) and the file is re-read per decision — config changes
take effect on the next open, no remount. Wired on BOTH mount surfaces (Linux
FUSE `fs_impl::lease_scope`, Windows `win_attach`), so a CfAPI attach and a FUSE
mount agree on who holds which pen.

### Phase 3 — High-availability ephemeral leases (SHIPPED) — kills the manual pen

The pen is now PROCESS-BOUND and self-freeing:

- **Bind**: every lease row records the owning `pid` (plus `project_id`/`device_id`
  for correct renewal context) — local store migration v3.
- **Short TTL**: 15s (`cairn_sync::LEASE_TTL_MS`). Correctness never depends on the
  TTL — fencing tokens do (SPEC §8 unchanged); the TTL is only how fast a DEAD pen
  evaporates.
- **Heartbeat**: the per-project runtime renews held leases every 5s
  (`LEASE_HEARTBEAT_MS`; 3 beats per TTL — two lost beats still renew) via the
  existing `Lease.Renew` RPC (renew does NOT bump the token; only takeover does).
- **Auto-release on close**: the editor closing the file releases the pen
  immediately (best-effort; failure is harmless, the TTL expires it anyway).
- **Reaper**: at every heartbeat, rows whose owning process is dead (machine-global
  `kill(pid,0)` / `OpenProcess` probe — the two tiny audited `unsafe` probes in
  `cairn-store`, same class as the eviction probes) are dropped locally AND
  best-effort server-released, so a crashed editor's pen frees in seconds on every
  peer's view.
- **Fenced renewal** drops the local row instead of lying: if a renewal fails with
  STALE_LEASE we were legitimately taken over; the next save re-acquires or surfaces
  a conflict. Never a silent overwrite (I2).

Result: no human in the loop for crashed, killed, or closed editors — the exact
"manual pen" the teams objected to is gone. The pessimistic lease remains the
correctness floor (it fences writers during the small windows where two writers DO
overlap), but its UX cost dropped from "manual unlock" to "≤15s wait".

### Phase 4 — Open-format semantic merging (v2 target, design only)

Three-way diff/merge is restricted to standardized interchange formats —
**OpenTimelineIO (OTIO)** and **FCPXML** — where base/ours/theirs has a defined
schema and GUID semantics. Reverse-engineering zipped `.prproj` XML schemas is
REJECTED: NLE point releases change schemas silently, internal GUID references break
under byte-level merges, and the failure mode is silent sequence corruption — the
one class of failure a sync product cannot survive. `.prproj`/`.drp` proprietary
merging is permanently unfavorable ROI at any fidelity we can ship.

A v2 OTIO merge design must at minimum: pin the OTIO schema version per merge,
resolve edit-order conflicts deterministically (same rule both sides: earliest
server seq wins ties after token fencing), and land merged results as a NEW journal
entry (never in-place) so the conflict-copy machinery remains the backstop.
The full design now exists: **ADR-0015** (`0015-otio-fcpxml-three-way-merge.md`) —
classifier table C0–C10, identity ladder, FCPXML lossiness ledger, and the v1
capture substrate (sidecar manifest, uuid stamps, base-pointer field, telemetry).

## Concurrency strategy matrix

| Strategy | Dev complexity | Data loss risk | Concurrency yield | Verdict |
| --- | --- | --- | --- | --- |
| Pessimistic lease (SPEC §8 floor) | Low | Zero | Low (1 writer/path) | **Keep as floor** (now ephemeral, Phase 3) |
| Native passthrough (Phase 1) | Low | Zero | High (vendor-native) | **Implemented** |
| Domain decomposition (Phase 2) | None (config file) | Zero | High (N writers, disjoint scopes) | **Implemented** (`.cairn-domains`) |
| Open-format merge — OTIO/FCPXML (Phase 4) | High | Low | Full (document-level) | v2 |
| Proprietary XML merge (`.prproj`) | Extreme | **Critical** | Full | **Rejected** |

## Implementation map

- `crates/cairn-sync/src/native_collab.rs` — Phase 1 detector (+ tests)
- `crates/cairn-store/src/db.rs` — migration v3 (`pid`, `project_id`, `device_id`),
  `LeaseRow`, `process_alive` probes
- `crates/cairn-cli/src/win_attach.rs` — passthrough stand-down, 15s TTL,
  pid-bound acquire, release-on-close
- `crates/cairn-cli/src/projects.rs::lease_keepalive` — 5s heartbeat + reaper
- `crates/cairn-sync/src/plane.rs`, `plane_grpc.rs` — `renew_lease` (renew RPC,
  no token bump)

## Consequences

- Leases acquired before this ADR (legacy rows with NULL pid) behave exactly as
  before: they expire by TTL. No migration action needed.
- The server is UNCHANGED on the wire: `Acquire/Renew/Release/ListLeases` already
  existed; pid/hostname are inherently device-local and never cross the wire.
- The `deny(unsafe_code)` policy gains one audited exception class: two
  process-alive probes in `cairn-store::db` (kill(pid,0), OpenProcess
  QUERY_LIMITED_INFORMATION) with inline SAFETY proofs — same discipline as the
  existing eviction probes.
- Two concurrent writers on the SAME sub-project file still serialize through the
  lease floor; making THAT fully concurrent is Phase 4's problem, and only for
  formats where the schema is a public contract.
