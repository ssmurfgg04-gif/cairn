# ADR-0031: Cross-device project state — what `.cairn*` holds, what syncs, and the path to synced collaboration

Date: 2026-10-08 · Status: accepted; **Phases 0–3 ALL landed 2026-10-08** (Phase 3: `.cairn` is a materialized view of the records — `cairn-cli/src/materialize.rs`) · Scope: cairn-sync, cairn-review, cairn-core, members/audit

## Context

The product treats review comments, member rosters, and audit decisions as
**project-level** facts: "the client left a note on v3", "Alice is editor
on Brand Film", "device X tried to detach". But the state actually lives
in the machine-local `.cairn` directory of each attached root:

```text
<root>/.cairn/review.json        version stack + guest links
<root>/.cairn/comments/v<N>.json frame-anchored notes
<root>/.cairn/members.json       the RBAC roster (the authority)
<root>/.cairn/audit.jsonl        every allow/deny decision
```

…and the sync engine **ignores all of it**. `cairn-core::pathutil::
is_ignored` refuses every path whose last segment starts with `.cairn`
(SPEC §10 ignore list, same rule as `.DS_Store`). Nothing else carries
these files: not the journal, not the plane, not the swarm.

The consequence is a semantic mismatch the review called a real
architectural gap: **project-level semantics on machine-local storage.**
Concretely, today:

- an editor's dashboard Team card shows only the roster of *their*
  machine's members.json (theirs is authoritative for enforcement, so
  this is worse than cosmetic);
- review comments published on machine A are invisible on machine B until
  A exports a markers file over HTTP (the ADR-0022 panel bridge);
- the guest-link revoke flow (mom-test P0) kills the link in *one*
  machine's review.json — the portal on another machine still honors its
  own copy;
- the audit trail is per-machine; there is no single ledger of who did
  what to the project.

This ADR exists because two honest-scope notes (BETA.md, review.rs module
docs) had drifted in opposite directions — one said the files "sync like
any other project file", the other that they are machine-local. The
machine-local one was true. Docs that contradict the code are bugs.

Also relevant: `.cairn/proxy-cache/` is a legitimate machine-local
*cache* (round 27 compress) — cheap to rebuild, device-specific codec
choices. Any cross-device plan must NOT start syncing it.

## Decision

A four-phase plan, ordered so every phase is independently shippable and
none requires a format break of the existing journal.

**Phase 0 — name the truth (shipped with this ADR).**
Every doc that claims review/member/audit state syncs now says it does
not, and points here. `review.rs` module doc fixed. The dashboard's
markers view documents the machine-local scope inline. Honesty first:
users make decisions ("can my co-editor see these notes?") on this answer.

**Phase 1 — project-scoped records under the engine (target: next round).**
Repurpose the *pattern the marker bridge already proved* (one payload
builder, three surfaces) for state: instead of files in `.cairn`,
represent rosters/audit/review as **journal-scoped project records** with
their own op type (`state_record`), CRDT-solvable merge semantics:

- members: LWW-register per device_id (conflict = two machines edited the
  same member's role concurrently — last-writer-wins is what every RBAC
  system does; the audit file retains the loser);
- audit: append-only set keyed by (ts, device, action) — merges by union,
  never conflicts;
- review versions: append-only list (the stack is append by contract);
  links/comments: append + tombstone (revoke = tombstone, which also
  fixes **cross-machine revoke** — Phase 2's P0).

The engine already handles keyed append-only streams with tombstones —
that is what file journals are. The records ride the existing WAL/outbox/
convergence machinery unchanged; kill -9 resume semantics carry over for
free (I2).

**Phase 2 — tombstone-backed revoke everywhere (the security payoff).**
Guest links move from "expiry + revoke in one review.json" to
"tombstone in the synced record set", so a revoke issued on any attached
machine propagates to every portal that reads project state. The portal's
RootProvider (already a service seam, ADR-0030) gains a
`link_revocations(project)` lookup consulted before minting/validating.

**Phase 3 — `.cairn` becomes a view, not a source of truth.**
The directory stays (NLE tools and the shell extension read review.json
locally; offline machines keep working), but it becomes a **materialized
cache** of the synced records, rebuilt on journal apply — the same
relationship the overlay state file already has to store metadata. Machine
caches (`proxy-cache`) stay outside the record set permanently.

## What we deliberately do NOT do

- **Sync the directory itself** (un-ignore `.cairn`). It would drag
  proxy caches across WAN, couple record writes to file-scanning
  cadence, and make "what state exists" depend on which machine scanned
  last. Records over files is the whole point.
- **Solve it in the dashboard** (round-trip through a "primary" machine).
  The review's UI-permission lesson applies: convenience is not authority.
- **Block the beta on it.** BETA.md's honest scope stands: single-machine
  review/roster today, the plan is public, the seam (ADR-0030 service
  layer) is already the right shape for the portal swap.

## Consequences

**Costs:** the journal gains an op family (schema version bump, PROTO —
versioned — already gates old clients); the review portal needs a record
back-end beside the file back-end during the transition; conflict UI for
role changes (rare, but "rare" is not "never").

**Wins:** cross-device revoke (a security property, not a feature);
one roster everywhere (RBAC stops being per-machine truth); audit becomes
a real project ledger; and the "same contract" goal from ADR-0020 — CLI,
panel and portal reading identical state — becomes true structurally
instead of aspirationally.

**Verification (Phase 0, this round):** `is_ignored` behavior asserted by
cairn-sync's scan tests (existing); docs cross-checked against code by
grep — no remaining doc claims `.cairn*` syncs.

## Phase 1 implementation notes (landed 2026-10-08)

The record family shipped as a `state_record` journal op (`PROTO_VERSION`
4 → 5). Decisions the plan glossed over, now written down:

- **Synthetic conflict keys.** The server's §7.1 conflict rule and journal
  index key on the `path` column, so every record carries a synthetic path
  `state/<family>/<key>` (`pathutil::state_record_path`). It satisfies
  `validate_rel_path`, gives per-key indexing for free, and keeps records
  out of every file scan (they exist only in the journal and the local
  table — `is_ignored` is not involved).
- **The §7.1 conflict rule does NOT apply to records** (deliberate SPEC
  §7.1 deviation, this ADR is the citation): concurrent same-key appends
  from different devices are legal because convergence is defined at apply
  time — members/review_link/review_comment are LWW-registers keyed by
  `record_id = key` (rank = `(ts_ms, device_id)`, and a tombstone beats a
  live value on an exact rank tie — mint+revoke inside one millisecond
  must not resurrect a link); audit/review_version are append-only unions
  keyed by a content id (`blake3(family|key|payload)[..32]`), idempotent
  under replay. Lease fencing also does not apply (no chunks to fence).
- **Compaction exempts `state/%` rows.** Snapshots stay file-only
  (`fold::materialize` ignores records), so a cold-attached device
  replays the full journal to rebuild record state; compaction therefore
  deletes nothing under `state/`. Record volume is small (~hundreds of
  bytes per entry); a per-key collapse (keep latest per key) is the
  Phase-2+ lever if a studio's audit ledger ever makes it matter.
- **Own-op suppression interaction.** `pull_phase` skips own-device
  entries, so a record is applied to the local table AT PUBLISH TIME
  (durable-before-send via the outbox, unchanged kill -9 semantics — a
  device that crashes before its record is acked re-sends it from the
  outbox, and the content-id dedupe makes that idempotent).
- **Enforcement stays machine-local until Phase 3.** Every roster/audit/
  review surface publishes fire-and-forget (a record failure never fails
  the primary action) and the dashboard Team view shows the synced
  roster BESIDE the local one; members.json remains the enforcement
  authority. Cross-machine enforcement is exactly Phase 2's revoke work.
- **Verification:** unit tests (LWW tie-break, union idempotence,
  tombstone), integration tests
  (`crates/cairn-sync/tests/state_records.rs` — two engines, one journal:
  LWW convergence, audit union, tombstone propagation), and sim-level
  E2E (`crates/cairn-sim/tests/state_and_offer_e2e.rs` — roster/audit/
  revoke over the real server journal, crash-resume durability for a
  pending record). Old-client behavior: unknown oneof arms decode to
  `None` and are skipped with the cursor advanced; servers must be
  upgraded first (an old server would re-encode the decoded op and drop
  the unknown arm — the deployment note in the proto file).
