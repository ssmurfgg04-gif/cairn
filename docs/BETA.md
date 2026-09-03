# Cairn Beta — 5 Minute Test (Windows)

The whole test: install one binary, attach a folder, open a real project file
in Blender (or Resolve), save, verify. Everything — the storage server, the
sync daemon, the CLI — ships in the single `cairn.exe`; for the beta the whole
stack runs on your machine over localhost, so nothing leaves the box.

## 1. Install (one command)

PowerShell:

```powershell
irm https://raw.githubusercontent.com/ssmurfgg04-gif/cairn/main/install.ps1 | iex
```

The installer detects your Windows version (10/11) and edition, downloads the
latest `cairn-windows-*.exe` from GitHub Releases, verifies it against the
release's SHA256 file, adds it to your PATH, and runs `cairn init` (creates
`%USERPROFILE%\.cairn`). If SmartScreen ever asks about the downloaded file:
**More info → Run anyway**.

## 2. Start the stack (two terminals)

Cairn syncs through a storage server; for the beta it runs on your machine.
Keep both windows open while you test — and if "two terminals" annoys you,
that is exactly the kind of feedback we want.

```text
Terminal A:  cairn server --data-dir %USERPROFILE%\.cairn-server --dev-insecure
Terminal B:  cairn daemon
```

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

## 6. Report back (the human part)

That hour of real use is worth more than 100 hours of CI. Note down:

- **what broke** — error text, what you clicked first
- **what was slow** — open? save? the initial sync?
- **what confused you** — any word or screen you had to think about twice

Send it with `cairn doctor --json` output and the daemon window's last lines.

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

## The zero-terminal path (round 12: tray + installer)

The 5-minute guide below is the CLI path — for power users, servers, and
anyone curious about the machinery. Everyone else installs and lives in the
tray:

1. **Install (one command, no admin):**

   ```powershell
   irm https://raw.githubusercontent.com/ssmurfgg04-gif/cairn/main/install.ps1 | iex
   ```

   SHA-verified engine + tray, per-user autostart, desktop shortcut, tray
   starts immediately.

2. **Day 2 operation is four clicks:** tray icon → Connect to Project…
   (folder picker — attach, scan, and mount run in the daemon) → Status
   Details (the doctor, "Everything is OK") → Open Project Folder. The tray
   polls every 3 s; the icon tooltip is the sync state.

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
