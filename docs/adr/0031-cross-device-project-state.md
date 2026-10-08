# ADR-0031: Cross-device project state — what `.cairn*` holds, what syncs, and the path to synced collaboration

Date: 2026-10-08 · Status: accepted (plan; Phase 0 groundwork landed) · Scope: cairn-sync, cairn-review, cairn-core, members/audit

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
