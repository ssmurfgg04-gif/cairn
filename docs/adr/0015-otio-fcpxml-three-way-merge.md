# ADR-0015: OTIO/FCPXML three-way timeline merge (Phase 4 design, v2-by-design)

Date: 2026-09-02
Status: Implemented (round 12, 2026-09-03): crates/cairn-tl — exact-rational
core, identity ladder, C0–C10 classifier, three-way merge driver + report,
FCPXML bridge + lossiness ledger, sidecar; `cairn tl-capture` / `cairn
tl-merge` (exit codes 0/1/2/3); golden corpus C0–C10, two-editor
simulations, 200-case property suite, python-otio interop oracle in CI.
The "v1 substrate first / v2 merge later" split collapsed into one shipped
implementation (the checklist round demanded it); the ADR's design carried
through unchanged, including the total classifier and the honesty policies.
Supersedes: none (design child of ADR-0014 §Phase 4)
Related: ADR-0014 (phase strategy), docs/research/2026-09-02-cdc-collab-scan.md §2 (substrate scan),
SPEC §7.1–7.2 (journal/fold), SPEC §8 (leases & fencing), docs/design/write-back.md

## Context

Phases 1–3 of ADR-0014 shipped: vendor-native passthrough (`.prodsys`, declared
Resolve collab), config-enforced per-subproject domain leases (`.cairn-domains`),
and pid-bound ephemeral leases (15s TTL, 5s heartbeat, reaper, release-on-close).
The manual pen is gone for crashes, kills, and closes. What remains is the honest
residue: two legitimate writers, same timeline document, serialized by the lease
floor, where a takeover produced a conflict copy and the system hands a human the
pen one last time.

ADR-0014 already fixed the boundaries of the remaining solution: semantic merge is
restricted to standardized interchange formats — **OpenTimelineIO (OTIO)** and
**FCPXML** — where base/ours/theirs has a defined schema and GUID semantics.
Reverse-engineering zipped `.prproj` / `.drp` vendor schemas is permanently
rejected (silent sequence corruption is the one failure class a sync product
cannot survive). The research scan (2026-09-02) settled the substrate: OTIO is the
neutral merge surface (ASWF-maintained, schema-versioned, defined edit model of
Clip/Gap/Stack/Track, deterministic JSON serialization); FCPXML is the secondary
entry format via a bridge; generic text-3-way tooling (git mergetools et al.) is
the wrong tool because it cannot see track/gap/edit structure.

This ADR is the design for that merge. It stays **v2-by-design**: v1 ships only
the capture substrate (a sidecar manifest, stable identity injection, one additive
journal field, and a conflict-class histogram) so that when v2 lands, every
timeline that ever flowed through Cairn is mergeable retroactively — base blobs
are content-addressed and never pruned.

## Goals / Non-goals

Goals:
- Deterministic three-way merge of OTIO timeline documents (FCPXML enters via the
  bridge, merges on OTIO only).
- A total conflict classifier: every op pair maps to exactly one verdict
  (auto-apply, auto-with-note, or human escalation) — no silent loss (I2).
- Rides the existing machinery end-to-end: journal ops, CAS refs, fencing tokens,
  snapshot/restore, conflict-copy backstop.

Non-goals (v2 and possibly forever):
- Vendor-native parsing/merging (`.prproj`, `.drp`) — rejected in ADR-0014.
- Effect-parameter math beyond the whitelisted attribute table (no NLE-internal
  color-science reconciliation).
- Real-time co-editing. This is asynchronous three-way merge after the fact.
- Being the FIRST line of concurrency defense. Phases 1–3 remain the defense;
  merge only shortens the human-pen tail.

## 1. Data model & capture — the v2-by-design contract for v1

Timeline documents are ordinary synced files. v1 adds four capture pieces:

1. **`.cairn-timeline` sidecar manifest** (one per timeline document, synced like
   any file): format tag (`otio-json` | `fcpxml`), OTIO schema version, FCPXML
   major version when applicable, and the adapter build that produced the stamps.
   Written by the capture adapter; read by the v2 merge to pin versions. Mixed
   versions across base/ours/theirs refuse to merge (escalate with artifacts) —
   honesty over guessing.
2. **Stable identity injection**: the capture adapter stamps
   `metadata.cairn.uuid` (uuidv7) on every OTIO `Composable` (Clip/Gap/Track/
   Stack) and every Marker at capture time. Identity then survives renames and
   moves. FCPXML input is bridged to OTIO first, then stamped — we never diff
   vendor XML.
3. **Base availability**: already true by construction — every prior synced blob
   is content-addressed in CAS and journal compaction (§7.1) removes journal
   ENTRIES, never CAS objects. The base document is the manifest hash the path
   held at the last sync point both saving devices agreed on; it is always
   fetchable. No retention change needed; this ADR records the invariant.
4. **Conflict-copy base pointer**: the one additive change — the conflict-copy
   journal op gains an OPTIONAL field carrying the base manifest ref, so the
   three-way input triple is reconstructible from the journal alone. Additive and
   wire-compatible; old clients ignore it.

## 2. Merge surface & algorithm (v2)

1. **Normalize**: parse base/ours/theirs with the pinned OTIO schema version from
   the sidecar. Canonical OTIO JSON: sorted keys, RationalTime in rationals
   (float seconds forbidden — float drift would break byte-determinism), stable
   child ordering as authored.
2. **Identity ladder** (per composable, strongest first):
   (a) `metadata.cairn.uuid`; (b) OTIO `name` + parent-path; (c) content
   fingerprint (media reference hash + in/out); (d) unlabeled-and-contentless =
   position-only identity (weakest — any structural op touching it escalates).
   The ladder is itself total and deterministic; a collapse at step (d) is
   conflict class C10.
3. **Flatten to op sets**: each side diffs against base into typed ops per TRACK
   (Stack→Track→child index is the semantic coordinate system):
   `INSERT(element, track, index)`, `REMOVE(element)`, `MOVE(element, track,
   index)` (recognized as remove+insert of one identity within one side),
   `TRIM(element, in_delta, out_delta)`, `ATTR(element, key, value)` over a
   whitelisted attribute table (opacity, speed, enabled, name),
   `MARKER_ADD/MARKER_REMOVE`, `TRACK_ADD/TRACK_REMOVE/TRACK_REORDER`.
4. **Cross-classify** by identity. Disjoint op sets auto-apply in deterministic
   order (ours-ops in base order, then theirs-ops in base order). Overlapping ops
   hit the classifier table (§3).
5. **Land**: merged document becomes a NEW file staged as `.name.merged.otio`,
   then a NEW journal upsert + commit; both inputs remain in CAS history; the
   machine-readable report (per-op verdicts, class histogram) syncs as
   `.cairn-timeline/reports/<seq>.json`. Never an in-place edit of either input
   (ADR-0014) — the conflict-copy machinery stays the backstop.
6. **Determinism**: `merge(base, ours, theirs) -> (merged, report)` is a pure
   function; same inputs → same output bytes. Which side is "ours" is decided by
   the fencing token (SPEC §8): the save made under the SURVIVING fence is ours;
   a zombie save whose lease was taken over mid-write is not a legitimate side at
   all. The ADR-0014 tie-break (earliest server seq wins) governs op ordering
   inside rebasing, never verdicts.

## 3. Conflict classifier (the total verdict table)

| Class | Situation | Verdict |
| --- | --- | --- |
| C0 | Op touches only ours xor theirs | auto-apply |
| C1 | MARKER_ADD on both sides | auto (union, ours order then theirs) |
| C2 | ATTR same element, different keys | auto (both) |
| C3 | ATTR same element, same key, different values | **human** — no last-write-wins on creative parameters |
| C4 | MOVE vs MOVE, different targets | **human** |
| C5 | MOVE vs TRIM (same element) | auto — commute is well-defined: MOVE, then TRIM |
| C6 | REMOVE on both sides | auto (remove once) |
| C7 | REMOVE (one side) vs TRIM/ATTR/MOVE (other) | **human** — deletion-wins is NOT safe for creative work |
| C8 | INSERT both sides at the same (track, index), different content | auto-with-note — ours at index, theirs immediately after; verdict recorded in the report |
| C9 | TRACK_REMOVE vs any op inside that track | **human** |
| C10 | Structural mismatch (schema-version skew, identity-ladder collapse) | refuse merge, hand over both documents + report |

The table is a Rust `match` over a closed enum — the compiler enforces totality,
and a Kani harness proves the classifier is total and panic-free over the bounded
op model.

## 4. FCPXML mapping

- Ingest: FCPXML → OTIO through the bridge mapping table; the sidecar records the
  FCPXML major version. A **lossiness ledger** is maintained per version (auditions,
  some compound-clip internals, roles → metadata, multicam angles → stacked-track
  approximation): anything outside the ledger MUST roundtrip or the merge refuses
  (C10). The ledger ships as a tested fixture, not prose.
- Merge happens ONLY on OTIO. Re-export is adapter-side: the NLE imports the
  merged OTIO; Cairn never edits vendor files in place.

## 5. Interplay with leases (Phases 1–3)

Merge is the LAST resort in a fixed order: Phase 1 passthrough (vendor
arbitrates — Cairn never merges vendor-arbitrated state either) → Phase 2 domain
scopes (no overlap) → Phase 3 ephemeral leases (serialize) → conflict copy →
Phase 4 merge offered. The Phase 3 UI signal extends with the merge offer
("conflict copy on `timeline.otio` — preview merge / keep both"); admin override
stays human. Merge never bypasses fencing: it runs only after the fence decided
which saves are legitimate.

## 6. Implementation map (v2) and v1 prerequisites

v2:
- `crates/cairn-tl/` — new crate: identity ladder, op extraction, classifier,
  canonical serializer. Pure (`#![forbid(unsafe_code)]`), no I/O, golden- and
  Kani-testable; `cairn tl-merge` ctl subcommand with `--dry-run` report mode.
- Adapter re-export path (`cairn tl-capture --emit fcpxml`).

v1 (shipped now, zero merge logic):
1. Journal conflict-copy op gains the optional base manifest ref (§1.4).
2. `.cairn-timeline` sidecar format defined here; `cairn tl-capture` stamps
   identities and writes the manifest.
3. Conflict-class telemetry: the fold path already sees conflict copies; v1
   counts them per class C0–C10 (verdicts computable cheaply without merging by
   running the classifier on captured triples when available). This histogram
   decides v2 prioritization on evidence, not intuition — we do not yet KNOW that
   C7 is the common case; we will measure it.

## 7. Test plan

- **Golden corpora**: one base/ours/theirs triple per class C0–C10 with expected
  verdict and merged output, committed as fixtures (real NLE exports where
  licensable, synthetic otherwise — labeled honestly per fixture).
- **Property/fuzz**: seeded random op sequences; invariants: no element
  disappears unless REMOVE on both sides; verdicts mirror-stable under
  ours/theirs swap; `parse ∘ build` is identity on the pinned schema version.
- **Kani**: classifier totality + panic-freedom on the bounded op model;
  commutation-table exhaustiveness.
- **Interop**: CI job roundtrips canonical JSON through the OTIO Python reference
  implementation.

## Consequences

- v1 carries a small additive journal change, a sidecar format, one adapter
  subcommand, and counters — no merge behavior until v2 enables it per-format,
  gated on the `.cairn-timeline` manifest's presence and version pins.
- Merge is report-only by default in v2; auto-apply requires operator config.
  The lease floor stays the correctness mechanism; merge only shortens the tail.
- Risks, named: identity drift across NLE round-trips (mitigated by uuid stamps +
  the fingerprint ladder); float drift in trims (eliminated by RationalTime
  rationals in the canonical form); 100k-clip features (merge is O(elements) —
  flatten once into hash maps, never a quadratic document diff).
- If the v1 telemetry shows conflict copies are already rare enough post-Phase-3
  (histogram near zero across studios), v2's ROI shrinks — and that is a
  legitimate, measured outcome of this design, not a failure of it.
