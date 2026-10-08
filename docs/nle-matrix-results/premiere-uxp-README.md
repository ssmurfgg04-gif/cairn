# premiere-uxp — the Premiere-host marker-bridge cell (CONTRACT-DEBT #3)

Status: **AMBER — until-first-run.** No transcript exists yet; the cell
stays amber until a real Premiere host produces one
(`premiere-uxp/transcript.json`, checked in here and uploaded as the
`premiere-uxp-transcript` artifact). There is no index table in
`docs/nle-matrix-results/` today (only per-run JSONs) — this README is
the cell note; when the matrix index appears, add the row:
`premiere-uxp | amber (until first real host run) | this file`.

## What the cell verifies (the acceptance, mirrored from docs/CONTRACT-DEBT.md #3)

The marker bridge has three surfaces: the CLI export and the loopback
`GET /api/v1/markers` endpoint share one payload builder
(`handoff::markers_payload`) and are verified. The third surface — **a
real Premiere host driving the UXP panel end-to-end** — has never been
verified. This cell is green when the transcript shows **the panel
fetching markers for the named project and receiving 200 with a
parseable FCPXML**. Job definition: `premiere-uxp` in
`.github/workflows/nle-matrix.yml` (dispatch-only,
`[self-hosted, windows, premiere]`, 15 min timeout, non-blocking).

## What must be on the self-hosted runner for the job to run green

1. **Adobe Premiere Pro 25.0+** (the host the panel manifest declares,
   `crates/cairn-cli/assets/uxp-panel/manifest.json` — `minVersion`
   25.0.0), licensed.
2. **UXP Developer Tool** (install from Creative Cloud) to load the
   panel in dev mode — no signing needed — *or* copy the panel from the
   repo's panel directory **`crates/cairn-cli/assets/uxp-panel/`**
   (`manifest.json` + `panel.html` + `panel.js`) to Premiere's UXP
   extensions dir: `%APPDATA%\Adobe\Adobe Premiere Pro\extensions\cairn-markers\`
   (panel README step 1).
3. **cairn CLI on PATH** (`CAIRN_BIN` overrides) and the **daemon
   running** (`cairn daemon`) so the loopback gateway
   `http://127.0.0.1:17778` — the exact endpoint the panel reads
   (ADR-0009 posture) — is up, with the named project attached and at
   least one published review version (the panel lists versions via
   `/api/v1/review`).
4. **Python 3.10+** on PATH for the collector.
5. The runner **registered with the `[self-hosted, windows, premiere]`
   labels** (no runner carries them yet — the job documents intent).

## How a run happens

- **CI (scaffold):** `workflow_dispatch` with input
  `premiere_project` = the attached project id. The collector replays
  the panel's exact fetch (`GET /api/v1/markers?project=<id>&version=<v>&format=fcpxml`,
  the URL shape `panel.js` builds) and records the human rows as
  UNCONFIRMED — so a pure CI run can never turn the cell green by
  itself. Verdict `pending-first-real-host-run`, exit 3 (non-blocking,
  `continue-on-error: true`).
- **Operator (the real run):** on the box, with the panel open inside
  Premiere, run:

  ```
  python scripts/nle_matrix_collect.py --host premiere --nle premiere \
    --project <project-id> --confirm-row P1 --confirm-row P2 --confirm-row P3
  ```

  P-rows (the human half of the acceptance): **P1** panel loaded in
  Premiere's UXP runtime; **P2** panel fetched markers for the named
  project and rendered the table (HTTP 200 inside the panel); **P3**
  exported FCPXML imported back into the open timeline. The
  transcript's verdict turns `green` (exit 0) only when the fetch leg
  is 200 + parseable FCPXML **and** P2 is confirmed — commit the
  transcript here.

## Known open question the first real host run answers

The dashboard P0 gate requires a per-launch token on every `/api/`
call (`x-cairn-token` header or `?t=`; the token is injected into the
loopback-served page as `window.CAIRN_TOKEN`). `panel.js` sends **no
token** today — its README advertises "no secrets: the panel is a
viewport". If the real host shows 403s, that is a genuine finding the
transcript is designed to surface (the fix is panel-side, out of scope
for this scaffold); the collector itself scrapes the token from the
loopback page so its fetch leg can verify 200 + parseable either way.
