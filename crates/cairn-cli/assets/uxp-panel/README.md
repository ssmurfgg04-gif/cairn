# cairn markers — the Premiere UXP panel (NLE marker bridge)

The round-16 ledger entry "compiled NLE panel" lands here: comments made
in the review portal, back in the timeline as markers — from *inside*
Premiere, without a terminal.

## What it is

A UXP panel (Premiere Pro 25+, no CEP, no Node panel legacy):
`manifest.json` + `panel.html` + `panel.js`. Zero dependencies, no build
step, no secrets. It talks to the daemon's loopback gateway
(`http://127.0.0.1:17778` — ADR-0009 posture: loopback only, the panel
is a viewport like the browser console):

* `GET /api/v1/status` — daemon version line
* `GET /api/v1/review` — projects + published versions (label, true fps)
* `GET /api/v1/markers?project=&version=&format=fcpxml|otio|csv` — the
  **same body** `cairn review export-markers` writes (one code path, the
  panel and the CLI can never disagree; markers carry the version's TRUE
  rate — the 25 fps / 23.976 drift fix of round 18)

Exports: FCP7 XML (Premiere/Resolve/FCP import), OTIO (canonical
timeline with markers), CSV (frame/TC/author/status/note). Saving uses
the UXP file picker inside Premiere; in a plain browser the panel falls
back to a download (that's how it's dev-tested against a mock daemon).

## Install (dev mode — the honest path for now)

1. Copy this folder to Premiere's UXP extensions dir:
   `%APPDATA%\Adobe\Adobe Premiere Pro\extensions\cairn-markers\`
2. Premiere Pro 25+ → *File > Developer > UXP Developer Tool* (install
   it from the Creative Cloud app if absent) → **Load** the manifest,
   enable **Developer Mode**, then *Plugins > cairn markers*.
   (Test mode: the panel also loads as an unbundled folder via the
   UXP Developer Tool's "Load" — no signing needed.)
3. Start the daemon on this machine:
   `cairn daemon` (the console the panel reads is the same one at
   `http://127.0.0.1:17778`).

## Honest scope

* **Verified**: panel logic live against the loopback API (headless
  Chromium against a mock daemon — project/version pickers, marker
  table, all three exports); the `/api/v1/markers` endpoint is unit- and
  dogfood-tested (same payload body as the CLI export).
* **Not yet verified**: an actual Premiere Pro 25 host — the UXP
  manifest + `require("uxp")` save path compile against the documented
  API but need a real box (the `premiere` CI matrix candidate, named in
  the round-19 ledger).
* No marker *push* into the open timeline yet — the export-then-import
  round trip is the v1 shape; the UXP DOM API for Premiere timelines is
  the follow-up (and it is what "compiled panel" ultimately means).
