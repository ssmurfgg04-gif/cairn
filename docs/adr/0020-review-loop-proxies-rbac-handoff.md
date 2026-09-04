# ADR-0020: The review loop, proxies, roles, and the sound handoff (Round 16)

Status: Accepted
Date: 2026-09-04
Supersedes: none (extends ADR-0009, ADR-0014, ADR-0017, ADR-0018, ADR-0019)

## Context

Round 15 closed the transport/usability gaps. The remaining critique was
product-shaped, and it was correct: *editors don't wake up wishing their
files synced faster — they wake up needing client notes.* The five gaps
that make Frame.io and Resolve the reference standard:

1. **Client review portal** — no-account guest links, frame-accurate
   comments, version stacks, presence. The #1 bottleneck in post.
2. **Proxy workflow** — nobody scrubs 50 GB ARRIRAW over café Wi-Fi.
3. **NLE depth** — comments must flow back into the edit as markers, not
   a browser window next to Premiere.
4. **Role-based permissions** — studios have hierarchies; the lock system
   needs to know who may lock what.
5. **AAF/OMF handoff** — the sound team must never cut against the wrong
   picture-lock version.

## Decision

**One architectural move, applied five times: every new feature is
files-in-the-root.** Review sessions, comment sets, proxy indexes,
membership, and handoff ledgers all live under `.cairn/` as deterministic
JSON, journaled and P2P-synced like any media file. Zero new transport
code; the swarm carries review state at LAN speed with zero cloud bytes;
merge semantics come from the existing machinery (notes three-way merge
included). The web surface is a thin local server over those files.

### §1 Client review portal (`cairn-review`)

- **Version stack** — append-only; guests land on the newest version,
  older versions stay reachable (the Frame.io stack model). Publishing
  never mutates history.
- **Guest links, no accounts** — the token IS the identity: uuid-v4
  (122 bits of OS CSPRNG) with a role (`commenter`/`viewer`), TTL, and
  optional latest-only scope. Every route resolves the token first and
  fails closed (404) on unknown/expired.
- **Frame-accurate comments** — comments are `cairn-tl` NoteSets (one
  per version, `.cairn/review-notes/v{N}.json`): blake3 content ids
  dedupe double-submits, the round-14 three-way merge converges
  offline edits, anchors are exact integer frames, and timecodes are
  NDF (23.976 counts on the 24 basis, 29.97 on the 30 basis).
- **Presence** — in-memory heartbeats (90 s staleness). Presence is a
  live signal, not state; it is deliberately never synced.
- **Player** — self-contained HTML/CSS/JS (no build toolchain, no CDN),
  served token-gated. HTTP-range media serving: capped 8 MiB `206`
  windows (memory flat at any file size) + streaming `200`. Scrub
  timeline, frame stepping, comment pins, version selector, resolve
  actions, presence rail. `?full=1` opts into original media.
- **Binding policy** — the portal listener is OFF by default
  (`cairn daemon --review 0.0.0.0:17778`) and runs detached: a portal
  failure never takes the sync engine down. This intentionally differs
  from ADR-0009's loopback dashboard — guests are the point.

### §2 Review CLI

`cairn review publish/link/list/comments/resolve/export-markers`.
Publish binds the version to the timeline digest (blake3 over the
canonical serialization) — which the handoff ledger verifies against
(§6). Links print the URL the editor sends to the client.

### §3 Proxy workflow (`cairn-proxy`)

A proxy is an **ordinary project file** under `.cairn/proxy-cache/`,
keyed by the blake3 digest of its source: journaled, synced, pinnable.
Remote machines pull megabytes because the proxy is the only small file
in the set; the 50 GB original stays a cold on-demand recall. "Smart
syncing" is the existing sparse/pin model — proxies give it something
light to sync; no new fetch-priority code.

- Pluggable `Transcoder`: `FfmpegTranscoder` (1080p H.264, downscale
  only, `-movflags +faststart` so the player scrubs over ranges) and
  `CopyTranscoder` (CI/test double; never for real media).
- `.cairn/proxies.json`: digest → entry (Ready/Stale/Failed),
  idempotent regeneration, stale-on-edit detection.
- The review portal streams the proxy by default (`stream_rel`).

### §4 RBAC (`cairn-core::rbac` + `cairn member`)

- 7 roles (owner / lead-editor / editor / assistant / colorist /
  sound-designer / reviewer) × 14 permissions — data, not code paths:
  the Lead locks the timeline, the Assistant organizes bins, the
  Colorist grades, the Client comments.
- `.cairn/members.json`: synced, deterministic, **device-keyed**
  (renames never change access). Unlisted devices stay `editor` — the
  fail-open two-person default; adding members makes a studio
  STRICTER, never looser. Bootstrap = whoever writes the first owner.
- Enforcement at every root-based mutating CLI surface (review
  publish/link: ManageReview; handoff record: Snapshot; member edits:
  ManageMembers — owner-only). `cairn member check` gives scripts an
  exit-code answer. **Daemon-side gRPC enforcement lands with the ctl
  proto change** (follow-up ledger) — today the matrix is advisory at
  surfaces this repo controls, not a sandbox against a hostile CLI.

### §5 NLE marker bridge (`cairn-tl::markers`)

Comments flow BACK into the edit: `notes_to_otio` attaches every
comment as a 1-frame `Marker.1` on the tracks stack (integer-exact
RationalTime — re-import lands on the identical frame the client
clicked); `notes_to_fcpxml` emits FCP7 XML markers Premiere, Resolve,
and FCP all import. `cairn review export-markers --version N` is the
one-liner. A compiled C++/UXP Premiere panel remains out of scope for
a Rust workspace (tracked honestly in the ledger); the marker bridge
delivers the workflow without the plugin.

### §6 AAF/OMF handoff ledger (`cairn-tl::handoff`)

Every export is recorded against (a) the blake3 digest of the exported
FILE (re-export/tamper detection) and (b) the **timeline digest**
(blake3 over the canonical serialization — the whole tree, because
anything that would change the export changes the binding). `verify`
returns Current | FileChanged | **TimelineMoved** — the last one is the
revolt-prevention signal: "the cut moved after the handoff". Magic
sniffing: AAF = CFB `D0CF11E0`, OMF = `OMFI`.

## Non-goals

- No accounts, no cloud, no telemetry — tokens are local state.
- No video codec in-process (ffmpeg is shelled out; the audit surface
  stays tiny).
- No compiled NLE plugin this round (marker interchange covers the
  workflow; the panel is ledgered).
- RBAC is not a security boundary against a hostile local CLI; it is
  toe-stepping prevention at the surfaces studios actually use.

## Test evidence

- `cairn-review`: 17 unit tests (stack append-only, NDF timecode math,
  token entropy/expiry fail-closed, role gating, range serving,
  presence, corrupt-file fail-closed) + live smoke: comment@960 →
  `00:00:40:00`, 206 ranges, 404 on bad tokens, resolve round-trip.
- `cairn-proxy`: 11 tests (idempotence, stale-on-edit, missing-proxy
  regeneration, escape refusal, corrupt index fail-closed) + live CLI
  smoke (generate → status → list).
- `cairn-core::rbac`: 4 tests (matrix hierarchy, member roundtrip,
  fail-open default, parse spellings).
- `cairn-tl::markers`: 4 tests (1-frame sorted markers, XML escaping +
  capping, canon round-trip, negative-frame clamp).
- `cairn-tl::handoff`: 4 tests (magic sniff, idempotent ledger,
  FileChanged/TimelineMoved detection, corrupt fail-closed) + live
  smoke: record → CURRENT → edit cut → TimelineMoved (exit 1).
- `cairn-cli`: member CRUD enforcement, fps parsing, civil-date math.

## Follow-up ledger (honest)

- Daemon-side RBAC enforcement (ctl proto: role on the device token).
- Compiled Premiere/Resolve panel (UXP/CEF) — the marker bridge covers
  the workflow first.
- Drop-frame timecode for 29.97 broadcast workflows (NDF today).
- Proxy auto-generation hook on `review publish` (today: explicit
  `cairn proxy generate`).
- Presence relay through the swarm (today: per-daemon only).
