# ADR-0021: The Cairn design system (Round 17)

Status: Accepted
Date: 2026-09-04
Supersedes: none (extends ADR-0009, ADR-0016, ADR-0020)

## Context

Round 16 delivered the product surfaces (review portal, dashboard, tray) with
the taste-skill "warm monochrome" profile: correct discipline, flat finish.
The verdict on the design comparison was blunt and fair: *"the gap is not
code, it's design system."* Users cannot tell a daemon is alive from a flat
dark table; a client cannot tell a review portal is trustworthy from a
debug-grade player. The directive: steal the reference app's surface language (aurora,
glass, one blue, DM Sans, the 0.22/0.68/0.35/1 easing) while keeping the
taste-skill rules that prevent slop (one accent, hairlines, tabular numerals,
honest empty states, transform/opacity-only motion, WCAG-AA text pairs).

## Decision

**A single token set, three surfaces, one accent.** The system is CSS-only —
no build step, no framework, no CDN dependency (the review portal is served
to guests from the editor's daemon; the page must render offline). The
DM Sans webfont is an `@import` with a system-stack fallback: degrade, never
break.

### §1 Tokens (the shared spine)

| Token | Dashboard (light) | Review (dark) | Tray (native) |
|---|---|---|---|
| ink | `#0a0a0a / #45464b / #76777e` | `#f2f3f5 / #a6abb5 / #6f7680` | — |
| canvas | `#f5f6f8` + aurora | `#0b0c0f` + aurora (2% alpha) | — |
| accent | `#087cff` (large) / `#0059d1` (text+buttons, 6.3:1) | `#2f9bff` (7:1 on bg) | status dot colors |
| surface | glass `rgba(255,255,255,.66)` + `blur(18px)` | `rgba(17,19,24,.72)` + `blur(18px)` | — |
| hairline | `rgba(10,10,10,.08/.14)` | `rgba(255,255,255,.07/.14)` | — |
| semantics | ok `#0e9f5c` · warn `#b45309` · bad `#c22929` | ok `#35c07f` · warn `#e8a33d` · bad `#f2555a` | green/amber/red |
| motion | `cubic-bezier(.22,.68,.35,1)`, rise-in stagger 55ms | same, 40ms | — |
| numerals | `font-variant-numeric: tabular-nums` everywhere time or bytes render | same | — |

Two restrained aurora fields per surface: fixed, `pointer-events: none`,
`blur(18px)`, ~0.1 alpha (light) / ~0.06 (dark), 26–34s alternating drift —
the one place "reference-grade" is allowed to show, at one-tenth the volume.

### §2 The dashboard (light, loopback)

- Onboarding rail over real state: **1/3 Connect → 2/3 Sync → 3/3 Ready**
  derived from actual fields (roots attached → cursor moved + outbox drained
  → healthy, zero conflicts). The card dismisses itself only when the system
  actually is ready — never fakes progress.
- All server strings are HTML-escaped before `innerHTML` (loopback does not
  excuse an XSS-shaped habit; paths and error strings are attacker-shaped).
- Storage stats now poll their real endpoint (`/api/v1/storage`), disk line
  included; nothing is invented client-side.

### §3 The review portal (dark, guest-facing)

The Frame.io gap list, implemented in one page, no framework:

- **Three-column shell** — versions / player / notes; collapses to two, then
  one. Notes are a first-class rail, not an afterthought under the video.
- **Transport** — play/pause icon button, big tabular timecode +
  `frame N / total`, fps, buffered-range bar, hover-scaled marker pins.
- **J/K/L shuttle** — L doubles native rate (cap 8x); J is a synthetic
  rewind ticker (negative `playbackRate` is not portable); K stops.
  Arrows step frames (shift = ×10), ↑/↓ ~1s, Home/End, N focuses the
  composer, M mute, F fullscreen — the keys editors already own.
- **Zoom to 400%** — 1x/2x/4x cycle, drag-to-pan (clamped), double-click
  toggle, HUD readout, resets on version switch.
- **Honest waveform** — drawn from the real decoded audio only when the
  media (proxy) is ≤ 40 MiB; decode failure hides the canvas. No fake bars,
  ever. Played region tints accent as the playhead passes.
- **Notes rail** — avatar initials, timecode chips that seek, status
  badges, open/done filters, resolve/reopen, relative timestamps, composer
  with a live "note at TC" chip that tracks the playhead.
- Volume/mute preferences persist per browser; presence shows live
  peer positions (`v3 · 00:00:31:12`).

### §4 The tray (native, always-on)

The tooltip is the "I'm alive" light: plain-English states
("all files synced (N files)", "syncing — N chunks in flight",
"attention: …") refreshed on the existing 3 s `NIM_MODIFY` path; errors
clipped to fit the 128-char tip budget. No new plumbing — text discipline
only.

## Consequences

- Surfaces stay build-free and offline-capable; a designer can retune the
  whole product by editing two token blocks.
- Webfont loads only when online; offline renders in the system stack.
- The waveform costs one extra fetch of small proxy media; larger media is
  skipped silently (budget constant `WAVE_BUDGET`).
- The Tauri window wrap (native frame around these exact assets) remains
  the ADR-0016 follow-up; this ADR deliberately does not add that weight.

## Evidence

- `crates/cairn-cli/assets/dashboard/{app.css,index.html,app.js}` (onboarding,
  escape-hardened rendering, storage endpoint wiring)
- `crates/cairn-review/assets/{review.css,review.html,review.js}`
  (3-column shell, JKL, zoom, waveform, filters, composer TC)
- `crates/cairn-tray/src/tray.rs` (summary text)
- `node --check` clean on both JS files; all `getElementById` references
  cross-checked against markup.
