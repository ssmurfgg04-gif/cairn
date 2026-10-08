# Cairn Beta — Setup Guide (Windows)

The whole test: install one binary, attach a folder, open a real project file
in Blender (or Resolve), save, verify. Everything — the storage server, the
sync daemon, the CLI — ships in the single `cairn.exe`; for the beta the whole
stack runs on your machine over localhost, so nothing leaves the box.

**Honest scope note (read this before the checklist):** the installer + tray
cover the client side end to end (no terminal). The storage SERVER still
needs one terminal command in this beta — there is no hosted default server
yet, and joining a teammate's machine still needs the enroll/login step from
section 3. That is the gap between "5 minutes" and reality; it is the top
product priority, and this guide no longer pretends otherwise. If you are
ready to host the meeting point yourself (a small VPS, systemd or Docker,
TLS), the runbook is `docs/runbook-meeting-point.md` — it also documents
what is still missing (admin-scoped code minting, /healthz) so nothing here
overpromises.

## 1. Install (one command — or zero commands)

Two interchangeable paths, same layout (the release page has both):

**Double-click (no terminal at all):** download `cairn-setup-<tag>.exe`
from the GitHub release and run it. NSIS installer, per-user (no admin),
Apache-2.0 license page, Start-Menu shortcut, optional ffmpeg download
(SHA256-pinned, fails closed on mismatch), tray autostart, and the daemon
starts hidden at the end — the dashboard opens from the finish page. Details
+ the CI gate that tests a silent install/uninstall on a clean Windows
runner every tag: `installer/windows/README.md`.

**PowerShell:**

```powershell
irm https://raw.githubusercontent.com/ssmurfgg04-gif/cairn/main/install.ps1 | iex
```

Either way: the engine + tray land in `%LOCALAPPDATA%\Programs\Cairn`, the
installer verifies downloads against SHA256, adds the install dir to your
user PATH, and runs `cairn init` (creates `%USERPROFILE%\.cairn`). If
SmartScreen ever asks about the downloaded file (the installer is unsigned
in this beta): **More info → Run anyway**.

## 2. Start the stack (one terminal, beta only)

The tray starts and supervises the DAEMON for you (that part is zero
terminal). The beta storage server is the one exception — start it in a
single terminal and keep it open while you test:

```text
terminal:  cairn server --data-dir %USERPROFILE%\.cairn-server --dev-insecure
```

If "the server needs its own terminal" annoys you: correct instinct, that is
exactly the feedback we want — it is the known beta gap from the scope note
above.

## 3. Enroll and attach

```powershell
cairn dev-enroll-code --server 127.0.0.1:7443        # prints a one-use enr-... code
cairn login --server 127.0.0.1:7443 --code <that code> --name beta-box
cairn attach C:\Users\You\BetaTest
```

`attach` is the one that matters: it binds the folder as a project root and —
on Windows — registers it as a **CfAPI sync root**, so Explorer gets cloud
badges and the project files get placeholder/hydration treatment just like
OneDrive. (The device ID is issued by the server at `login`.)

## 4. Open Blender

File → Open → `C:\Users\You\BetaTest\scene.blend`
Scrub the timeline. Save. Close.
(Premiere: `.prproj`. Resolve: `.drp` — same idea.)

While the file is open, the daemon holds a **lease** on it: a second machine
opening the same project gets an EBUSY-style conflict instead of silent
last-writer-wins. On one box you won't see this — it's listed here so the
behavior isn't a surprise later.

## 5. Verify

```powershell
cairn status     # should show: 1 project, files synced, no pending outbox
cairn doctor     # every check ok
```

Stress it once: kill the daemon window mid-save, restart it, and watch
`cairn status` converge (crash-safety is a designed property, not a hope).

## 6. Why is the first upload slow?

Because the bytes have to move — that part is physics, not software. A 50 GB
project over a 20 Mbit/s uplink is hours on the first push no matter what
runs on either end. What Cairn controls is that you pay that cost **once
per unique byte** and that the work never evaporates mid-flight:

- **Chunked.** Files are split at content-defined boundaries
  (`cairn-core::chunker`, FastCDC-style; 16 MiB max chunks — `CHUNK_MAX`,
  `crates/cairn-core/src/lib.rs`), each chunk hashed (BLAKE3) and
  compressed (zstd, with a cross-device dictionary, ADR-0013).
- **Deduplicated.** The server answers "which chunks do I already have?"
  before you upload (bloom-filter pre-filter over its chunk table —
  `crates/cairn-server/src/upload.rs` `BatchExists`; a bloom false positive
  can never skip an upload, property-tested adversarially). A re-save of
  the 50 GB edit uploads the delta, not the file; a teammate attaching the
  same footage re-uploads none of the chunks the server already has.
- **Resumable.** Uploads ride persisted sessions that resume at chunk
  granularity across daemon AND server restarts (`crates/cairn-server/src/
  upload.rs` — session rows survive restarts; `crates/cairn-sync/src/
  outbox_worker.rs` retries the outbox with AIMD pacing). The crash-safety
  gate is a tested property: kill -9 mid-upload → resume → byte-identical
  (M3, verified at 512 MB; 5 GB-class soak behind `CAIRN_E2E_BYTES`).

What it does NOT do: make your uplink faster. For the first bulk load,
wire real bandwidth or co-locate with the meeting point.

## 7. Report back (the human part)

That hour of real use is worth more than 100 hours of CI. Note down:

- **what broke** — error text, what you clicked first
- **what was slow** — open? save? the initial sync?
- **what confused you** — any word or screen you had to think about twice

Send it with `cairn doctor --json` output and the daemon window's last lines.

## 8. Testing between two homes

The localhost beta above exercises one machine. The next honest step is two
machines in two places:

- **Host a meeting point** (`docs/runbook-meeting-point.md`): a small VPS
  runs `cairn server` (+ optionally `cairn signal` for the direct-peer
  swarm leg). Both homes `cairn login --server https://…` against it.
- **Automated coverage exists:**
  `crates/cairn-sim/tests/two_homes_e2e.rs` (landing this round) drives two
  full daemons — two "homes" — through the real server: initial upload,
  cross-machine edits, conflict truth, revoke propagation, with the network
  partitioned and restored. That proves the mechanics in CI.
- **What the E2E cannot prove:** home routers (CGNAT especially), Wi-Fi
  drops, laptops that sleep at 11pm, a 5 GB library opening for the first
  time over a real uplink, and whether a second human finds the flow
  obvious. Run the real two-homes test with a teammate before you trust it
  with production work — the automated suite is the floor, not the
  substitute. Report what breaks per section 7.

---

## Appendix: the same test without a human (headless Blender)

```python
# test_cairn.py
import bpy
bpy.ops.wm.open_mainfile(filepath="C:/Users/You/BetaTest/scene.blend")
bpy.context.scene.frame_set(100)  # Scrub
bpy.ops.wm.save_mainfile()        # Save
```

```powershell
blender -b -P test_cairn.py
echo $LASTEXITCODE   # assert in CI
```

Headless runs the full I/O path (open → read → seek → write → close) with no
GUI and exits with a code you can assert in CI. The catch: this validates
"does Cairn serve bytes correctly" — not "does a human editor find it smooth."
Headless catches the mechanical 90% of bugs; the human session above catches
the rest. Both matter; neither replaces the other.


---

## The zero-terminal path (tray + installer)

For the everyday flow you should never need the CLI at all. Everyone else
installs and lives in the tray:

1. **Install (zero or one command, no admin):** the double-click NSIS
   installer (`cairn-setup-<tag>.exe` on the release page — section 1) or:

   ```powershell
   irm https://raw.githubusercontent.com/ssmurfgg04-gif/cairn/main/install.ps1 | iex
   ```

   Both are per-user, register the tray autostart, and start the daemon
   hidden — the tray icon appears immediately (the installer's finish page
   opens the dashboard). install.ps1 SHA-verifies every download; the
   installer ships with a `.sha256` sidecar on the release (checking it is
   `sha256sum -c`, one command) — the workflow that builds it prints the
   hash in the job summary.

2. **Day 2 operation is four clicks:** tray icon → Connect to Project…
   (folder picker — attach, scan, and mount run in the daemon) → Status
   (a plain-language answer; the technical details land on your clipboard
   for a bug report) → Open Project Folder. The tray polls every 3 s; the
   icon tooltip is the sync state.

3. **Two editors, one timeline:** the merge is automatic for OTIO/FCPXML —
   when a conflict copy lands, `cairn tl-merge --base <ancestor> --ours
   <surviving-save> --theirs <earlier-save>` writes `<ours>.merged.otio`
   plus a machine-readable verdict report. Exit codes: 0 clean, 1 notes, 2
   conflicts (a human looks — the report names the pair), 3 refused
   (nothing touched). The tray surfaces conflicts through the status line;
   resolving them is still a deliberate act, never a silent pick.

4. **Explorer shows the truth:** synced files carry the in-sync badge; the
   root shows syncing/offline/error state from the daemon. Errors are
   sticky until the engine clears them.

What the tray will never be: a review tool, a media browser, or an editor.
It is a thin onboarding layer (ADR-0016) — the engine is the product.

### Studio hardware-gate (I1)

If you have a physical Windows box with Premiere/Resolve, run the collector
and send back the JSON — that report is the last open item in the shipping
matrix: `docs/design/nle-test-matrix.md` (procedure + minimum hardware
spec) and `scripts/nle_matrix_collect.py`.
