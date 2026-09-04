# ADR-0018: Frame-anchored review notes + timeline round-trip audit

Date: 2026-09-04
Status: Accepted (implemented this round)
Supersedes: none
Related: ADR-0015 (OTIO/FCPXML three-way merge — notes reuse its ladder philosophy),
ADR-0014 (leases — notes are comments ON the timeline, never write authority),
SPEC §3 non-goal amendment (review notes are editorial tooling, not a chat network)

## Context

Two standing complaints from every editorial round:

1. **Scattered feedback hell.** Notes about a cut live in Frame.io threads,
   email, WhatsApp screenshots, and sticky notes — none of which anchor to
   the timeline the way the NLE sees it. A "3 sec in, too dark" comment
   cannot survive a frame-rate conversion or a trim, and merging two
   reviewers' feedback by hand is assistant work.
2. **The round-trip audit.** Shipping a timeline to another NLE (or another
   facility) has one verification tool: a human eyeballing every speed ramp,
   title, and transition at 3am. "The XML ate my grade" is discovered during
   the online, when it is expensive.

Both are deterministic, mechanical jobs. Both should be code.

## Decision

### A. Review notes (`crates/cairn-tl/src/notes.rs`)

- A note is anchored to a **frame (exact rational `frame@rate`)** and
  optionally a **clip identity** (uuid ladder, name fallback — the ADR-0015
  ladder, so notes survive trims that move a clip within the timeline).
- **Content-derived ids**: `id = blake3(anchor_key ‖ body ‖ author)[0..16]`.
  This single choice gives the merge its editorial semantics:
  - an **edit is a new id** — old note vanishes, new one appears, both
    sides converge without ever mangling text mid-sentence;
  - a **status flip keeps the id** — a deterministic lattice decides
    (Resolved is sticky; Rejected-vs-Resolved is the one surfaced
    conflict);
  - **unchanged-vs-delete → deletion wins** (what a human removed stays
    removed); **edit-vs-delete → the edit survives** (it is a NEW note).
- The genuine conflict — same anchor, same author, different bodies — is
  reported as a `NoteConflict`, never silently resolved. That is the
  "two people answered the same client comment differently" case; no
  algorithm should pick a winner.
- Storage: a `.notes.json` sidecar (the timeline itself is never mutated —
  notes are opinions, not edits).
- **CSV interop** (`csv` submodule): import/export with the `Frame Number`
  column alias real review tools emit; timecode rendering at any rational
  rate. Frame.io out, notes merged, back into Frame.io — lossless enough
  for editorial work.

CLI: `cairn notes import|list|export|merge` (merge exits non-zero with a
table when conflicts exist — the human escalation contract mirrors
tl-merge).

### B. Round-trip audit (`crates/cairn-tl/src/verify.rs`)

`verify_roundtrip(source, roundtrip) -> VerifyReport` compares a timeline
BEFORE and AFTER it traveled, producing a checklist where every entry
names the element and the exact delta:

- clip inventory (count + identity via the uuid ladder, name fallback);
- **frame-exact duration drift per clip** — rational arithmetic, so a
  2400-frame clip that returns 2398 frames is a REAL number, not a float
  blur;
- per-clip **effect inventory** — dropped speed ramps, lost grades,
  vanished motion titles: the classic XML/OTIO round-trip casualties;
- markers, transitions (in/out offsets), gaps, track counts;
- audio media links per clip.

Severity contract: `Loss` = do not cut from this file; `Warn` = inspect
before trusting. The report serializes to JSON for CI gating.

CLI: `cairn tl-verify --base X --roundtrip Y [--json]` — exit 0 clean, 1
warnings, 2 losses (mirrors tl-merge's contract).

### C. Bin-locks ship alongside (ADR-0014 local pen)

`cairn lock/unlock --project --path` claim/release a visible write-authority
pen on a path (or directory prefix) so collaborators see "locked by
<device>" instead of discovering a conflict copy after a save. Notes and
locks are the two collaboration primitives the review flow actually needs;
neither touches the sync engine's conflict rules.

## Testing evidence

- notes: id-derivation round-trips; merge matrix (edit/new/status/
  delete-vs-unchanged/delete-vs-edit/same-anchor-conflict) pinned;
  CSV import→export→import byte-stable at the semantic layer; timecode
  rendering at 24/25/30000÷1001.
- verify: synthetic timelines with planted casualties (dropped effect,
  2-frame drift, lost marker, missing media link) each detected with the
  right severity + named detail; clean round-trip reports pass.
- CLI contract tests: exit codes per the table above.

## Consequences

- cairn-tl grows two pure modules (no I/O, no engine coupling — CLI edges
  only); CSV export is the interop surface other tools can generate
  against.
- Review notes are explicitly NOT a chat system: no transport, no
  presence, no read receipts — distribution rides files (or the swarm,
  ADR-0017, when the notes sidecar is a project file like any other).
- SPEC §3 gains the user-mandated exception row; ADR index updated.
