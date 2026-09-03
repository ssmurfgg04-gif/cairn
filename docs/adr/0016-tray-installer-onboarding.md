# ADR-0016: Clicky-Clicky onboarding — installer + system tray

Date: 2026-09-03
Status: Accepted (implemented this round)
Supersedes: none (shipping child of the round-12 checklist)
Related: ADR-0014 (leases/phases), ADR-0009 (dashboard — the only other UI),
docs/BETA.md, install.ps1, crates/cairn-tray, crates/cairn-fs-win/src/badge.rs

## Context

Cairn's engine is complete and headless by design: leases, crash-safety,
placeholder hydration, chunked sync, the three-way timeline merge
(ADR-0015). The honest gap from the 100% checklist is that a video editor
will not open a terminal. "Works in theory, unusable in practice" is a real
product failure, not a marketing one. Two things must be true for the
product to exist for its users:

1. One command (or one click) installs everything, verified.
2. Day-2 operation happens from the Windows notification area: status,
   connect, open, disconnect. No terminal, ever.

## Decision

### A. Installer (`install.ps1`, upgraded)

- Downloads BOTH release assets: `cairn-windows-<tag>.exe` (engine: CLI +
  daemon + server) and `cairn-tray-windows-<tag>.exe` (the tray).
- Each download is verified against its `.sha256` sibling asset (existing
  contract) and Mark-of-the-Web is cleared after verification.
- The tray asset is OPTIONAL: older/dev/partial releases without it
  install engine-only and SAY SO (degrade loudly, never half-install).
- Autostart: `HKCU\...\CurrentVersion\Run\CairnTray` — per-user, no admin
  rights, idempotent overwrite. A Windows SERVICE was rejected for v1: the
  daemon must run in the user session for keychain access (device token),
  and a service would need credential plumbing this round does not need.
  The Run key + tray-started `cairn daemon` keep the trust boundary simple.
- Desktop shortcut to the tray (`WScript.Shell` COM — stock Windows, no
  dependencies).
- Launches the tray immediately (a reboot is not part of onboarding);
  `CAIRN_INSTALL_NO_LAUNCH=1` lets the CI gate run headless.
- Explorer badge registration is NOT installer work: CfAPI sync-root
  registration and provider status are per-project runtime state the daemon
  owns at `cairn attach` (badge.rs). The installer never touches HKLM.

### B. Tray (`crates/cairn-tray`, new)

- Win32 system tray via windows-rs: `Shell_NotifyIconW` +
  `CreateIconFromResourceEx` (the .ico is embedded at build — no external
  files, no temp writes) + `TrackPopupMenu(TPM_RETURNCMD)`.
- **The hard boundary: the tray never links the engine.** No cairn-sync,
  no store, no gRPC, no cairn-proto. Every capability is a wrapped
  `cairn.exe` subprocess (`CREATE_NO_WINDOW` — no console flash), every
  read is `status --json` / `doctor` / `init --json` output. Rationale: a
  tray crash can never take sync down; the tray can be killed/restarted
  freely; the engine stays headless and testable; the binary stays ~1 MB.
- Menu (the checklist contract): status line · Connect to Project…
  (SHBrowseForFolderW → `cairn attach`) · Status Details (`cairn doctor`
  in a message box) · Open Project Folder (Explorer at the root) ·
  Settings (version/enrolled/home/project state) · Disconnect (`cairn
  detach`) · Exit.
- Poll cadence 3 s on a worker thread; the message loop never blocks on a
  subprocess. Login (enrollment code) stays a terminal/install-doc step in
  v1 — the tray surfaces "not enrolled" honestly instead of faking a flow.
- Non-Windows builds compile a stub main so the workspace stays
  cross-platform-green; the real binary builds on the windows-latest CI
  leg and ships as a release asset.

### C. Badge (Explorer state) — summary, decided in badge.rs

The badge decision table is a portable, unit-tested state machine; the FFI
(`CfUpdateSyncProviderStatus` + `CfReportSyncStatus` + per-file
`CfSetInSyncState` via the existing mark_in_sync calls) rides the SAME
CfAPI connection the write-back callbacks use. The daemon's sync loop
feeds it facts; only CHANGES hit the FFI. Errors are sticky until the
engine clears them (honest state, I2).

## Alternatives rejected

- **Windows service for the daemon**: credential plumbing for the user
  keychain; the Run key + user-session daemon is the honest v1.
- **Tauri/WinForms tray**: .NET/WebView runtimes for a four-item menu; the
  hand-rolled Win32 tray is ~600 lines, zero deps, single-file.
- **Tray linking the engine (in-process status)**: one crash domain,
  heavier binary, harder to test — the subprocess boundary IS the design.
- **MSI (WiX)**: an MSI for two exes + one Run key is ceremony; the
  PS-module installer is one file, already CI-gated end-to-end. An MSI can
  wrap the same script later for enterprise MDM without changing anything.

## Consequences

- Release artifacts double (engine + tray + their sha256s); the installer
  gate asserts BOTH install and the Run key.
- The tray is the second (and last) UI, alongside the dashboard
  (ADR-0009). Nothing else gets a GUI — no review workflows, no AI: the
  engine remains the product.
- Autostart can be disabled by deleting one registry value; the installer
  prints what it did.
- Risks, named: SHBrowseForFolderW is legacy-but-supported (the modern
  IFileDialog is COM ceremony for the same result); tray polling costs one
  short-lived subprocess per 3 s (measured ms-class, CREATE_NO_WINDOW);
  explorer-not-ready at login is retried once after 1 s (the existing
  NIM_ADD failure mode).
