# installer/windows — the Cairn NSIS installer

The zero-terminal Windows install: one `.exe`, double-click, done — no
PowerShell, no admin rights. `install.ps1` (repo root) stays as the scripted
path and produces the **same layout**, so both are interchangeable on the same
machine. Contract cairn.nsi shares with install.ps1 (never change one without
the other):

| What | Value | Where it comes from |
|---|---|---|
| Install dir | `%LOCALAPPDATA%\Programs\Cairn` | install.ps1 `-InstallDir` default |
| Autostart | `HKCU\...\Run` value **`CairnTray`** = `"<dir>\cairn-tray.exe"` | install.ps1 step 5 |
| Daemon start | hidden at install end, stderr → `%USERPROFILE%\.cairn\daemon.log` | install.ps1 step 6b |
| Dashboard | `http://127.0.0.1:17778` (opened by the finish page) | ADR-0009 loopback console |

## What gets installed

- `cairn.exe` — the engine: CLI + daemon + storage server + CfAPI glue in one
  binary (release.yml's build contract).
- `cairn-tray.exe` — the system tray; it supervises the daemon from every
  login (ADR-0029). Start Menu shortcut **Cairn** points here; optional
  Desktop shortcut (component, default off).
- **NOT bundled:** `cairn-app.exe` (the native console window, ADR-0022). It
  is the workspace-excluded Tauri crate: `cargo tauri build --bundles nsis`
  produces its own setup (`cairn-window-*-setup.exe` release asset), and
  install.ps1 chains it. Without it, the tray's "Open Console" falls back to
  the browser dashboard — the same surface (crates/cairn-tray/src/tray.rs).
- Optional component **ffmpeg** (ON by default): pinned gyan.dev essentials
  zip, SHA256-verified, `ffmpeg.exe` + `ffprobe.exe` extracted to
  `<install>\bin` (that bin dir goes on your user PATH, which is how the
  engine's `FfmpegTranscoder::detect_ffmpeg` finds it). Honest why: the proxy
  workflow (lightweight editing copies) and ffprobe-backed review publishing
  need it; without it, review publishing wants explicit fps/frames. Honest
  cost: ~115 MB download, ~200 MB on disk. The pin lives in `cairn.nsi`
  (`CAIRN_FFMPEG_URL` / `CAIRN_FFMPEG_SHA256`, verified 2026-10-08) and fails
  CLOSED on a hash mismatch — the component skips with a warning, the install
  never breaks. Update procedure is commented next to the defines.

## Build locally

From a Windows box with [NSIS 3](https://nsis.sourceforge.io) (makensis on
PATH) and a repo checkout:

```powershell
cargo build --release -p cairn-cli --bin cairn
cargo build --release -p cairn-tray --bin cairn-tray
```

⚠ Same trap as release.yml: if you set `RUSTFLAGS` yourself it **replaces**
`.cargo/config.toml`'s flags — include `--cfg tokio_unstable` or the build
breaks loudly (tokio io-uring, ADR-0025):

```powershell
$env:RUSTFLAGS = "-C target-feature=+crt-static --cfg tokio_unstable"
```

(crt-static is optional locally; CI uses it so the exe runs with no VC redist.)

Stage the bundle inputs (the `.nsi` reads everything from `.\dist`):

```powershell
# from the repo root
New-Item -ItemType Directory -Force installer\windows\dist | Out-Null
Copy-Item target\release\cairn.exe installer\windows\dist\
Copy-Item target\release\cairn-tray.exe installer\windows\dist\
Copy-Item LICENSE installer\windows\dist\LICENSE.txt
Copy-Item crates\cairn-tray\src\cairn.ico installer\windows\dist\
cd installer\windows ; makensis cairn.nsi
```

→ `installer\windows\cairn-setup.exe`. To stage binaries from a different
tree: `makensis /DSRCDIR=path\to\staged\dir cairn.nsi`.

Useful silent switches (all combinations valid):

- `/S` — silent install (daemon still starts hidden; browser does NOT open).
- `/NOFFMPEG` — deselect the ffmpeg component (CI uses this: a release gate
  must not depend on a third-party mirror's bandwidth).
- `/D=C:\some\dir` — install elsewhere (NSIS built-in; still user-scope).

## How the CI gate works (`.github/workflows/installer.yml`)

On every `v*` tag (and on demand via workflow_dispatch), windows-latest:

1. `choco install nsis -y`, then builds `cairn.exe` + `cairn-tray.exe` with
   release.yml's exact `RUSTFLAGS: -C target-feature=+crt-static --cfg
   tokio_unstable` (the env var replaces config flags — see the release.yml
   comment; this exact trap killed the v1.0.0 re-tag run).
2. Stages `dist\`, runs `makensis`, smoke-tests the staged binaries.
3. **Gate (the real test):** installs silently (`/S /NOFFMPEG`), asserts
   `cairn.exe` exists at `%LOCALAPPDATA%\Programs\Cairn`, waits for the
   installer-launched daemon to serve `http://127.0.0.1:17778` (200), runs
   `cairn status --json` against it, uninstalls silently (NSIS `_?=` form so
   the wait is synchronous), asserts the `CairnTray` autostart value is gone
   and the install dir is removed.
4. Prints SHA256 + build metadata (tag, commit, rustc, makensis) into the job
   summary — the light supply-chain artifact trail (review-item #70's full
   provenance chain is the tracked follow-up).
5. On tags only: uploads `cairn-setup-<tag>.exe` + `.sha256` to the GitHub
   release (waits for release.yml to create it if needed; `--clobber` makes
   re-runs idempotent).

## Signing status — honest note

**The installer is unsigned today.** SmartScreen will show "Windows protected
your PC" for an unsigned downloaded exe; the BETA guide already documents the
"More info → Run anyway" step for install.ps1 downloads, and it applies here.
Authenticode signing (an EV/OV cert, `signtool sign /fd SHA256` in the
workflow right after makensis, timestamping via RFC 3161) is tracked as
future work — review-item **#72** (the cargo-deny/SBOM/provenance/signing
cluster). The cert plugs in at exactly two places when it lands:

- `.github/workflows/installer.yml`, new step between `makensis` and the
  gate: `signtool sign /fd SHA256 /tr <RFC3161-TSA> /td SHA256 <file>` for
  `cairn-setup.exe` (secrets: the cert as a Base64 PFX + password), and
- `installer/windows/cairn.nsi` needs **no change** — signing is a
  post-build wrapper, which is why the gate asserts behavior, not
  Authenticode state.

The same step would sign the staged `cairn.exe`/`cairn-tray.exe` and (via
release.yml) the `cairn-window-*-setup.exe` Tauri bundle — Tauri reads
`TAURI_SIGNING_*`/certificate env vars, see its NSIS bundler docs.
