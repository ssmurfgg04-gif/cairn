# ADR-0023: Round 20 — the collaboration five (semantic merge, live presence, client changelist, timeline branches, clip search)

Date: 2026-09-05
Status: accepted (Round 20)

## Context

Round 19 shipped the marker bridge and the native window. The product gap
after dogfooding was not transport or sync speed — it was the five
collaboration mechanics that make a two-editor studio stop passing hard
drives and phone-calling each other:

1. **Zero-touch semantic timeline merge** — two editors re-cut the same
   clip; today every such pair escalates C3 (human). The mechanical,
   frame-disjoint cases (head vs tail) should land without asking.
2. **Live presence** — "are you working on Scene 3?" is a phone call
   because the other editor's playhead is invisible until save.
3. **The client-notes changelist** — 80% of client revision notes are
   mechanical ("cut 2 seconds off the end"); the editor pays full price
   for each one by hand.
4. **Timeline branches** — `Project_v3_experimental.prproj` sprawl; no
   safe way to experiment and steal the good parts back.
5. **Intelligent clip search** — finding "the worried closeup" in 50GB of
   BRAW means scrubbing bins by hand.

## Decisions

### §1 Semantic merge — opt-in policy, C11, the exact line

`cairn-tl::classifier` gains a `Policy` parameter:
`Conservative` (the default, bit-for-bit the Round-19 behavior — every
legacy test pins it) and `Semantic`. Under `Semantic`, ONE new verdict
class exists: **C11 — frame-disjoint re-cuts of the same element
auto-merge with a note**. The rule is exact and base-free: ours touched
ONLY the in-edge, theirs ONLY the out-edge (or vice versa). The deltas
compose mechanically (`apply_trim` already composes sequential trims).

**The line, drawn hard**: same-edge re-cuts ("one cut at 00:01:23, the
other at 00:01:24") stay C3/HUMAN under EVERY policy; C7 (delete-vs-edit)
and attr conflicts are NEVER relaxed. The map-union metadata rule was
considered and REJECTED: without threading base values into the
classifier, subkey edit-vs-delete is undetectable — a silent-loss hole.

**Opt-in is per-device, not per-project**: `--semantic` on `tl-merge`,
the `semantic_merge` daemon flag (default `false`). No role, no member
file, nothing project-wide. The main-pen-holder question stays answered
by the existing fencing + RBAC (LockTimeline/EditTimeline): semantic
policy changes WHO gets interrupted, never who holds the pen.

Reports record `"policy": "semantic"|"conservative"` — a C11 verdict can
only exist in a self-describing semantic artifact.

### §2 Live presence — ephemeral telemetry on the swarm

`PeerMsg::Presence` (tag 0x50): one opaque, ≤1200-byte app payload per
datagram over the SAME XChaCha20-Poly1305 sessions as block traffic
(direct or relay). Never persisted, never reassembled, never retried —
loss-tolerant by design (the next heartbeat supersedes).

Surfaces: `Swarm::broadcast_presence` / `subscribe_presence` /
`presence_snapshot` (last-event-wins, 15 s TTL); the daemon hub
(`projects::PRESENCE_TX`) fans out to the first ctl-side streaming RPC
(`CtlPresence.WatchPresence`) and the dashboard SSE (`/api/v1/live`) +
submit (`POST`). The UXP panel gets an opt-in toggle and a Premiere
playhead heartbeat (guarded `require("premierepro")`).

**Off by default, per-device**: the `live_presence` flag gates both
directions; the swarm reads it at join. Inbound presence on a disabled
node is dropped at the door — no state growth, no events, nothing
observable. RBAC: `Permission::ViewPresence` (all roles — presence is
coordination, not power; the flag is the real gate).

### §3 The client-notes changelist — the 3-step no-AI recipe

`cairn-tl::note_ops`: the client's pinned frame (the existing
`NoteAnchor`) + a keyword robot reading the body like a spreadsheet:
`cut/trim N (seconds|frames) off the end/start`, `delete`, `replace
with X`, `quieter/louder (±N dB)`. **The creative line is hard**: "make
it pop" parses to `Creative` — highlighted + timestamped in the portal,
the human decides. Parsing is derived at read time (deterministic,
nothing stored); the portal session API carries `parsed` per comment.

Renderers: JSON (the authoritative, machine-applyable form), CMX3600
EDL, FCP7 xmeml markers. **Applying is a separate explicit act**:
`cairn review apply-changelist` PREVIEWS by default (exit 1, nothing
written); `--yes` writes `<timeline>.changelist.otio` — never in-place.
The apply engine is TWO-PASS identity-based: pass 1 pins frame→element
references against the SOURCE cut (what the client watched); pass 2
applies by reference — a trim can shift every downstream clip, the
targets never drift (the mid-apply resolution bug the round-20 smoke
test caught and killed).

### §4 Timeline branches — git-for-video, foolproof by construction

`cairn tl-branch`: branches live in
`<timeline-dir>/.cairn-timeline/branches/` (local-first: `.cairn*` is
ignore-listed, SPEC §10 — branches are the editor's own sandbox; the
synced team-branch story is the named follow-up). Ledger
`branches.json` + `timeline.otio` + a FROZEN `parent.otio` whose digest
the ledger records and `merge` verifies.

- `create` copies IN (the working file is untouched)
- `checkout` copies OUT to `<name>.otio` — never clobbers
- `merge` is the cairn-tl three-way with the recorded parent as base;
  output `<target>.merged.otio` per ADR-0015 convention
- `cherry-pick --element <uuid|name>` steals ONE element, positioned
  after the last shared anchor
- `delete` is SOFT (trash/ + `restore`); only `purge --force` is forever

Names are validated tightly (no separators, no reserved words, ≤64
chars) — a branch name is a label, not a path.

### §5 Intelligent clip search — offline, deterministic, no AI

`cairn search`: two surfaces with one ranked token model (full-token >
prefix > name substring > path substring; all-tokens-matched bonus;
deterministic tie-breaks):

- **files** — the store's rows (or a raw `--path` walk)
- **clips** — every `*.otio`/`*.fcpxml` in the project parsed with
  cairn-tl; each clip indexed by name + media + exact rational timeline
  position. "worried closeup" finds `interview_worried_closeup.mov` AND
  the `scene3_v2.otio` range where it was cut in at 00:01:12.

No index is persisted — a bounded scan per query (≤200 timelines,
≤20 MiB each) is correct-over-stale at project scale.

## Honest scope (the named follow-ups)

- The engine's CONFLICT arm (ADR-0015 §1.4 base-ref field) still hands
  off to a human; the auto-merge OFFER inside the sync engine is the
  follow-up — the policy machinery is landed and tested, the wiring is
  not.
- Presence flag flips apply at swarm join (attach/daemon start); a live
  swarm restart on flip is the follow-up.
- Real-Premiere panel verification (licensed host, real timeline DOM)
  is the studio leg — the loopback contract is CI-green; the
  `require("premierepro")` heartbeat is shipped best-effort and guarded.
- The 1000-clip merge cross-classification is O(ops²); at 500×500
  interacting pairs it costs ~6.5 s on a 2-core dev box. An
  element-key index is the optimization follow-up.
- Team branches over the server refs (the `refs` table is multi-name by
  construction; only `main` is ever written) — the local-first ledger
  ships now.

## Consequences

- `classify_pair` takes a `Policy`; the Kani classifier harness proves
  BOTH policies (concrete loop, unchanged state space)
- class C11 is wire-stable (reports/telemetry distinguish semantic
  auto-merges from C3 conflicts without ambiguity)
- `Permission::ViewPresence` joins the matrix (17 → 18 permissions; all
  roles) — STATUS/ADR-0020's "14 permissions" line was already stale
- `CtlPresence` is the first ctl-side server-streaming RPC; the
  additive-surface rules (UNIMPLEMENTED degradation, fields 1–5, no
  100–199) follow the frozen-contract recipe
- The installer/release pipeline is unchanged: tag `v*` still builds
  engine + tray + window and runs the installer gate end-to-end
