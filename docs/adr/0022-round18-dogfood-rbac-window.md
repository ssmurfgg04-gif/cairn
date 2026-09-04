# ADR-0022: Round 18 — the dogfood/RBAC/window round

Date: 2026-09-04
Status: accepted
Supersedes: none (extends ADR-0016, ADR-0020, ADR-0021)

## Context

Round 17 shipped the design system. The follow-up directive named three
legs, in order, for one week:

1. **Dogfood the review loop live** (`cairn review publish → guest link
   → comment → export-markers → NLE`) — "fix what feels off before you
   wrap it";
2. **Enforce RBAC at the daemon's ctl boundary** — "without daemon-side
   enforcement, an artist can `cairn detach --project a`";
3. **Wrap the hardened flow in a native window** — the ~6 MB Tauri
   shell, after the dogfood, "not the 16-line stub we had".

Plus a 12-point UI audit (empty state, fonts, preview, words, members,
file badges, quota, search, i18n, update, help) and the WAN leg:
two studios across the internet, not loopback.

## Decision

### 1. Dogfood findings are bugs, and they were real

`scripts/dogfood_review.sh` (26 live assertions, real ffmpeg media at
23.976 and 25 fps) caught five:

* **Marker TC drift** (the headline): `export-markers` hardcoded
  rate 24. A 25 fps cut landed markers 1.7 s late per minute; a 23.976
  cut carried `ntsc=FALSE` (a frame of drift every ~42 s). The rate now
  comes from the review session's TRUE rational
  (`fcpxml_rate_fields` + `notes_to_otio_at`): PAL stays `ntsc=FALSE`,
  1001-rates `TRUE`, OTIO markers carry the true rate with the frame
  index unchanged.
* **Guest-link visibility leak**: `latest_only` links could stream,
  comment on, and resolve HIDDEN older versions — the routes resolved
  `version()` directly instead of `versions_for(&link)`. All four routes
  now fail closed with 404 (never 403 — never confirm hidden versions
  exist).
* **Dead player on a missing proxy**: a promised-but-absent proxy 404'd
  the media route. The portal now falls back to full-res media and the
  session reports `proxy_ready`, which the player renders as an honest
  "serving full res" chip.
* **Odd-dimension proxies failed outright**: `scale=-2` only evened the
  WIDTH — a 321x179 source died in x264. Both axes are forced even with
  `setsar=1`; a real-ffmpeg regression test proves it.
* **Publish ergonomics**: `--fps`/`--frames` are now optional (ffprobe
  fills them from the media — a wrong hand-counted number silently
  corrupts every comment TC bound after it), and `--proxy` defaults to
  auto-generation at a new 720p review profile (the 1080 default
  produced a proxy 95.5% of source; the review profile lands at 27.9%
  in the dogfood run).

### 2. RBAC at the boundary, with a synced audit ledger

Three new permissions (`AttachRoot`, `DetachRoot`, `ManageFlags`) join
the matrix, and `rbac_guard()` runs in EVERY mutating ctl handler
(attach, detach, set-flag, snapshot, restore, pin, unpin, recall) plus
the dashboard's HTTP equivalents. The acting device is the daemon's
enrolled identity (the machine is the actor); the project's synced
`members.json` is the authority; corrupt members fail CLOSED.

The audit ledger (`.cairn/audit.json`) records every guard decision on
the machine that made it — content-derived entry ids, bounded to the
newest 500, atomic writes, and the dashboard Team tab renders the trail.
**Honest scope, corrected during this round:** `.cairn*` is on the scan
ignore-list (SPEC §10), so the ledger is LOCAL per machine — as is the
round-16 review/members/proxy state, whose "syncs with the project"
claim was an aspiration the transport never honored. What the ledger
already fixes: RBAC decisions that vanished into daemon logs now land
in a durable, bounded file. Machine-to-machine audit (and synced review
state) needs that state moved to a synced path with append-only
merge semantics — named in the follow-up ledger below.

### 3. The window: Tauri 2, standalone on purpose

`crates/cairn-app` points the OS webview (WebView2/WKWebView) at
`http://127.0.0.1:17778`. No plugins, no updater, no IPC, no bundled
browser: the window is a viewport onto a surface the daemon already
serves, so ADR-0009's loopback posture is unchanged. The crate is a
workspace `exclude`d standalone (its dependency tree is Tauri's; the
linux gates have no webkit2gtk) and CI compiles it on `windows-latest`
(`tauri-check`). Tray stays with `cairn-tray` (ADR-0016); its Open
action gains "launch cairn-app, else browser".

### 4. The UI audit's 12 points

Dashboard: nav renamed to the editors' vocabulary (**Locks**,
**Versions**, display names); the onboarding wizard appears ONLY on the
zero-root state (the design review was blunt: a wizard above a DIT's
sync metrics treats them like a novice); a Files tab with per-file
synced/syncing/conflict + pinned + placeholder badges; a Team tab
(roster, my role, join-code invite, audit trail); header search across
projects/files/reviews; a quota meter with the above-95% warning; the
honest update chip; the `?` help overlay with g-key navigation and
scroll-spy; i18n (EN/DE/JA/ZH, ~90 keys); non-blocking font loading.
Review portal: the `?` keyboard map, the proxy chip, TC weight.

A two-round VLM design review graded the result 8/9/8 ("hire-worthy")
and its round-1 findings (wizard placement, search-field weight,
hydration null glyph, table/side contrast, nav scroll-spy) were applied.

### 5. WAN: the machinery existed, the wiring did not

The swarm had STUN + punch + relay since round 15 — but `SwarmConfig.stun`
was hardwired `None` in the CLI: WAN was dormant by omission. Now the
swarm join resolves a default public STUN list (Cloudflare first) unless
the home pins an override (`cairn daemon --swarm-stun host:port |
--swarm-no-stun`, persisted in `swarm/stun`); `docs/runbooks/wan-p2p.md`
is the two-studio runbook (signal+relay on a VPS, join code, punch,
relay fallback, expectations per NAT class).

Tray: transition balloon toasts (daemon lost / new error / sync
completed — silence otherwise), Windows-rendered as toasts.
Shell extension: an "Open (default editor)" context item executing the
file association (never a hard-coded NLE path).

## Consequences

* The review loop's second half (export → NLE markers) is now
  frame-exact by construction, not by convention: three regression tests
  + the dogfood script pin the rate math.
* Denials are visible studio-wide: the synced audit ledger means an
  owner on another machine sees the same trail (and the dashboard Team
  tab renders it).
* `cairn-app`'s standalone status means it does not ride the workspace
  gates: the `tauri-check` job is its only compile gate — a red there is
  the contract, not an accident.
* `.cairn*` is ignore-listed: review/members/proxy/audit state is
  machine-local TODAY. Moving it to a synced path (append-only merge for
  the ledger, NoteSet-style convergence for review state) is the round's
  biggest honest correction and the follow-up with the most product
  value — the cross-editor comment convergence ADR-0020 promised does
  not exist yet.
* The `swarm/stun` meta is a small credentials-free config surface; the
  WAN runbook assumes a VPS for signal+relay (~$5/mo, bandwidth-metered)
  — direct WAN punch is best-effort by NAT class and the relay is the
  floor, not the exception.
* The round's own CI landing caught a REAL convergence defect the sim
  never modeled: a stat-only drift (mtime moves, content does not) made
  the §7.1 guard refuse a remote upsert and consume it with no
  re-delivery — the device held its old head forever, silently, with no
  conflict copy (nle-matrix W4, one firing in ~7 runs, timing race).
  apply_entry no longer clobbers the row's disk stat on same-manifest
  no-op replays (that clobber FAKED the drift), and process_file
  short-circuits identical-content re-hashes into the fork-point
  re-delivery (the conflict_copy mechanism, minus the copy). The exact
  on-runner toucher for the observed firing is unidentified (CfAPI
  placeholder conversion and metadata-normalizing I/O are the
  candidates) — the fix makes ANY stat-only touch converge, which is
  the property the guard needed all along. Two regression tests pin
  both halves.
* Honest ledger for next round: live Resolve import of the exported
  FCP7/OTIO markers (a human run), the cairn-app NSIS build + tray
  launch wiring on a real Windows box, `cairn update check` writing the
  update meta the dashboard now reads, and waveform peaks for >40 MiB
  media (server-side peaks) remain.
