# ADR-0028: Note-shape v2 — ranges, pins, annotations, per-note visibility

Date: 2026-09-05 · Status: accepted (implemented round 25) · Scope: cairn-tl, cairn-review, cairn-proto sidecar contract, CLI exports

## Context

ADR-0018 froze review notes at their v1 shape: frame-anchored
(`NoteAnchor { clip?, frame, rate }`), content-addressed
(`id = blake3(anchor_key ‖ 0x1F ‖ body ‖ 0x1F ‖ author)[0..16]`), lifecycle
`Open | Resolved | Rejected`. That freeze was correct — the id is the
cross-device merge key, and every downstream surface (review portal rows,
hashtag chips, `?t=&v=` deep links, CSV export, the Premiere marker bridge)
derives from it.

ADR-0026 then shipped the Frame.io-style review surface and hit the wall this
ADR answers: the four features reviewers actually ask for — **a comment that
spans a region, not one frame** (ranges), **a marker pinned to a spot on the
frame** (pins), **a drawn overlay** (annotations), and **studio-internal
notes the client never sees** (per-note visibility) — all require *persisted
fields*. Frontend-only hacks were rejected there for the right reason: any
field that affects rendering but not identity would desync merge keys and
silently corrupt mechanical-note parsing and every existing id.

So v2 is a protocol round, not a UI round: the sidecar schema, the id
material, the review-portal filter boundary, and the export CLI all move
together, versioned so v1 notes never rewrite.

## Decision

### A. Versioned id material, additive migration (no rewrites, no collisions)

The id formula gains a literal version tag as the FIRST hash element:

```
v1:  blake3(anchor_key ‖ 0x1F ‖ body ‖ 0x1F ‖ author)          (unchanged)
v2:  blake3("note2" ‖ 0x1F ‖ anchor_key ‖ 0x1F ‖ body ‖ 0x1F ‖ author
           ‖ 0x1F ‖ kind ‖ 0x1F ‖ range_key ‖ 0x1F ‖ visibility)   [0..16]
```

- **v1 notes keep their v1 ids forever.** Nothing rewrites; every existing
  sidecar, export, and deep link stays valid. v1 is a strict subset of v2:
  a v1 note *parses* as v2 with the default envelope (`kind=Comment`,
  `range=[frame,frame]`, `visibility=Public`, no attachment) while retaining
  its original id string. Lazy migration — zero migration scripts.
- **No accidental collision** between the shapes: v1 material begins with
  `clip:`/`frame:`; v2 begins with the literal `note2`. A v2 note cannot be
  mistaken for a v1 note by hash accident, and a v1 reader that encounters a
  v2-only field set treats the row as foreign schema and skips it (the same
  tolerance the sidecar parser already has for foreign entries).
- **Writers choose per note**: a plain frame comment (the overwhelmingly
  common case) is still written as v1 — smallest representation, broadest
  compat. A note carrying any v2 feature is written as v2. The writer, not
  the reader, bears the version cost.

### B. Ranges

`NoteAnchor` gains an optional inclusive frame range:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub range: Option<(i128, i128)>,   // (start_frame, end_frame), inclusive
```

- A point note IS the degenerate range `[f, f]`; the anchor keeps `frame`
  for v1 compat and computes `range_key = "{start}:{end}@{rate}"` for the v2
  id material.
- **Merge key stays anchor-first**: clip identity if present, else the range
  start. A range note and a point note on the same clip deliberately land in
  the same merge bucket — that is where a conflict entry is *useful* (two
  editors talking about the same clip region).
- The review portal renders a range note as a bracket on the scrub bar and
  the seek target becomes the range start; `?t=` deep links are unaffected
  (they carry media timestamps, not note ids).

### C. Pins (kind, on-frame position)

A `NoteKind` discriminator enters the v2 material:

```rust
pub enum NoteKind { Comment, Pin, Annotation }
#[serde(default, skip_serializing_if = "Option::is_none")]
pub pin: Option<(f32, f32)>,       // normalized 0.0..=1.0 (x, y) on the frame
```

- `Pin` = frame-anchored marker with a position: the Frame.io "drop a pin on
  the frame" gesture. Body may be empty (a pure marker) — v1 required a body
  for the id material; v2 hashes the (possibly empty) body the same way, so
  an empty-body pin still has a stable id.
- The **Premiere marker bridge** (ADR-0022 round) is the natural consumer:
  pins map 1:1 to NLE markers (comment = marker with duration 1 frame;
  range = marker with duration; annotation = marker + attached reference
  clip). `export-markers` gains pins without a schema change on that side.

### D. Annotations (content-addressed attachment)

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub attachment: Option<String>,    // BLAKE3 hex of the overlay blob
```

- An annotation's drawing is a PNG overlay stored as an ordinary blob in the
  project CAS — chunked, content-addressed, deduped, hydrated by the
  machinery that already exists. The note references it by hash; the blob
  is project data, not a new storage subsystem.
- Rendering: the portal composes the attachment over the frame at `pin`
  (centered if `pin` is absent). The portal's media proxy pipeline already
  produces frame-accurate stills, so the overlay path reuses seek+still.
- **I2 applies**: the blob's BLAKE3 is verified on ingest like every other
  blob; a corrupt annotation overlay surfaces as `CHECKSUM_MISMATCH`, never
  as a silent render failure.

### E. Per-note visibility (the RBAC boundary moves to the data)

```rust
pub enum NoteVisibility { Public, Internal }
```

- `Internal` notes sync to studio devices (the sidecar is device-trust
  scoped — same as today's notes) but are **filtered at the review-portal
  boundary**: the portal's HTTP summary and the reviewer-facing note lists
  never include them for client-audience links. Enforcement is server-side
  (cairn-review http layer), never client-side hiding — a reviewer with the
  link literally never receives the bytes, so a devtools snoop finds
  nothing.
- `export-markers` and CSV export gain `--visibility {public|internal|all}`
  (default `public`, matching the "what the client gets" mental model).
- The dashboard's open-notes count and the portal's per-review counts count
  **public** notes only when the audience is a client link — the same number
  the client would see. Studio views count all.

### F. What deliberately does NOT change

- The proto surface (`proto/cairn/v4`): notes ride the sidecar, not the
  protobuf; the review summary rows stay aggregate counts. No wire re-plumb.
- The same-anchor-same-author collision rule (ADR-0018): v2 keeps surfacing
  it as a conflict entry — the id material now includes kind/range/
  visibility, so two editors' notes at the same anchor diverge *more*
  gracefully (different range ⇒ different id ⇒ both survive as distinct
  notes; identical everything ⇒ same id ⇒ dedup, which is correct).
- Status semantics (`Resolved` sticky across merges) untouched.

## Migration & compat summary

| reader ↓ / writer → | v1 note | v2 note |
|---|---|---|
| v1 reader (today's binaries) | full | skips (foreign schema) |
| v2 reader | full (defaults envelope) | full |

No rewrites, no migration scripts, no id churn. The first v2 writer is the
review portal's own compose path (pin/range UI), so the format only appears
where the features exist.

## Testing plan (acceptance gates for the implementation round)

1. **Id partition**: proptest — no v1 id ever equals a v2 id for identical
   anchor/body/author; v2 ids differ when any of kind/range/visibility
   differ.
2. **Round-trips**: v2 note through sidecar serialize→parse→merge is
   id-stable; v1 fixture (the ADR-0018 corpus) parses with the default
   envelope and its original id.
3. **Merge semantics**: range-note + point-note on one clip → same bucket,
   conflict surfaced; distinct ranges → both survive.
4. **Visibility boundary**: portal HTTP handler test — internal notes
   absent from client-audience responses, present for studio roles
   (RBAC-aware test harness, mirroring the flag/RBAC tests in
   `cairn-server`).
5. **Export gates**: `export-markers --visibility public` excludes internal
   (golden file), `all` includes.
6. **Annotation I2**: tampered attachment blob fails verification and the
   note renders its fallback (missing-overlay affordance), never a crash.

## Consequences

- The four reviewer-facing features become data, not UI state — they sync,
  merge, and survive device churn like every other note.
- The v1 freeze is honored rather than broken: compatibility is additive and
  collision-proof by construction, at the cost of a permanently two-shaped
  format (accepted: plain comments stay the common case and stay v1).
- The review portal's server-side filter becomes a security-relevant
  boundary (internal notes must never leak to client links) and inherits the
  RBAC test discipline the flags path already carries.
- Implementation landed in round 25: the versioned id material and lazy v1
  envelope (`cairn-tl/notes.rs`, proptest-partitioned), the field-wise
  pin/attachment merge, the portal compose path (pin / range / internal),
  `GuestRole::Studio` as the internal-visibility audience, the
  hash-verified attachment endpoint (`GET /r/:token/attachment/:hash`,
  I2), `export-markers --visibility`, and the v2 CSV columns. All six
  acceptance gates above are wired as tests (`notes.rs`, `properties.rs`,
  `http.rs`, `handoff.rs`). The note-id freeze documented in ADR-0026 is
  lifted *only* by this versioned extension, never by an unversioned
  field addition.
