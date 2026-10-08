; Cairn — Windows NSIS installer (round 31, ADR-0016 "clicky-clicky" completion).
;
; The zero-terminal path: double-click this .exe, never open PowerShell.
; install.ps1 remains the scripted path ( irm ... | iex ); this installer
; packages the SAME layout so both paths stay interchangeable:
;
;   install dir:  %LOCALAPPDATA%\Programs\Cairn        (per-user, no admin;
;               case-insensitive on Windows — the same spot install.ps1 uses)
;   binaries:     cairn.exe (CLI + daemon + server, one binary) + cairn-tray.exe
;               (the tray supervises the daemon, ADR-0029)
;               NOTE: cairn-app (the native console window, ADR-0022) is a
;               workspace-EXCLUDED Tauri crate with its own NSIS bundle
;               (release asset cairn-window-*-setup.exe, built by
;               `cargo tauri build --bundles nsis` in release.yml). Nesting one
;               NSIS setup inside another is how install.ps1 chains it; we do
;               NOT bundle it here — without it the tray's "Open Console"
;               falls back to the browser dashboard (same surface,
;               crates/cairn-tray/src/tray.rs). Documented, not hidden.
;   autostart:    HKCU\Software\Microsoft\Windows\CurrentVersion\Run value
;               "CairnTray" — the EXACT key name + quoted-exe value shape
;               install.ps1 writes (install.ps1 step 5). Never rename one
;               without the other.
;   daemon start: hidden at install end, stderr -> %USERPROFILE%\.cairn\
;               daemon.log — install.ps1 step 6b parity. The daemon
;               self-dedups (probes :17778 before bind, exits 0), so a
;               pre-existing daemon is never double-spawned (round 27).
;   ffmpeg:       OPTIONAL component (ON by default): pinned gyan.dev
;               essentials zip, SHA256-verified, extracts ffmpeg.exe +
;               ffprobe.exe into <install>\bin (the engine's
;               FfmpegTranscoder::detect_ffmpeg resolves PATH binaries).
;
; EMBEDDED-POWERSHELL RULE: NSIS expands $Name inside EVERY string kind
; (backticks included), so each PowerShell variable is written $$name —
; a bare $p here would be an NSIS "unknown variable" compile error.
;
; Build (see installer/windows/README.md):
;   makensis cairn.nsi            (from this directory; inputs under .\dist)
; CI: .github/workflows/installer.yml builds + gate-tests + attaches to releases.

Unicode true
ManifestDPIAware true

; ---- inputs (staged by CI, or by the local-build steps in the README) -----
!ifndef SRCDIR
  !define SRCDIR "dist"          ; .\dist\cairn.exe, cairn-tray.exe, LICENSE.txt, cairn.ico
!endif

; ---- pinned ffmpeg component ----------------------------------------------
; Verified 2026-10-08: gyan.dev release-essentials rolling zip, which contained
; ffmpeg 9.0.2 at pin time. The SHA256 is the real supply-chain pin (light
; version of review-item #70): if gyan.dev rotates the file, the hash check
; FAILS CLOSED — the component skips with a warning and the rest of the
; install proceeds (ffmpeg is optional; the proxy workflow simply reports it
; missing, service/actions.rs degrades to "publish with explicit frames/fps").
; TO UPDATE THE PIN:
;   1. download the URL below,
;   2. sha256sum (or `certutil -hashfile <file> SHA256` on Windows),
;   3. replace both defines, bump the "Verified" date comment,
;   4. re-check the zip layout still has ffmpeg.exe/ffprobe.exe under
;      <top>/bin/ (the extractor resolves the top-level dir automatically).
!define CAIRN_FFMPEG_URL       "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip"
!define CAIRN_FFMPEG_SHA256    "60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba"

!include "MUI2.nsh"
!include "WinMessages.nsh"       ; ${HWND_BROADCAST} / ${WM_SETTINGCHANGE}
!include "Sections.nsh"          ; ${SF_OFF}
!include "FileFunc.nsh"          ; ${GetOptions} for /NOFFMPEG

; ---------------------------------------------------------------------------
; General
; ---------------------------------------------------------------------------
!define PRODUCT_NAME       "Cairn"
!define PRODUCT_VERSION    "4.0.0"          ; workspace version (Cargo.toml)
!define PRODUCT_PUBLISHER  "Cairn contributors"
!define PRODUCT_URL        "https://github.com/ssmurfgg04-gif/cairn"
!define UNINST_KEY         "Software\Microsoft\Windows\CurrentVersion\Uninstall\Cairn"
!define AUTORUN_KEY        "Software\Microsoft\Windows\CurrentVersion\Run"
!define AUTORUN_VALUE      "CairnTray"      ; MUST match install.ps1 step 5

Name "${PRODUCT_NAME}"
OutFile "cairn-setup.exe"
; Per-user: NO admin required. (Power users: run elevated and add /D=D:\somewhere
; — NSIS's built-in /D override — to install elsewhere; everything user-scope
; keeps working because we only ever write HKCU + the chosen dir.)
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\Cairn"
ShowInstDetails show
ShowUnInstDetails show
SetCompressor /SOLID lzma

Icon   "${SRCDIR}\cairn.ico"
UninstallIcon "${SRCDIR}\cairn.ico"

; ---------------------------------------------------------------------------
; MUI2 pages: License (Apache-2.0) -> Components -> Directory -> Install ->
; Finish (opens the dashboard). Languages: English ships the strings we wrote;
; DE/JA/zh-CN are stock NSIS language files (trivially available, kept).
; ---------------------------------------------------------------------------
!define MUI_ICON   "${SRCDIR}\cairn.ico"
!define MUI_UNICON "${SRCDIR}\cairn.ico"

!define MUI_ABORTWARNING

!define MUI_WELCOMEPAGE_TITLE "Cairn — local-first media sync"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${SRCDIR}\LICENSE.txt"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES

; Finish page: "Run" opens the dashboard (the daemon itself was already
; started hidden by the core section — install.ps1 step 6b parity).
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_FUNCTION "OpenDashboard"
!define MUI_FINISHPAGE_LINK "Beta guide (5-minute test)"
!define MUI_FINISHPAGE_LINK_LOCATION "https://github.com/ssmurfgg04-gif/cairn/blob/main/docs/BETA.md"
!insertmacro MUI_PAGE_FINISH

; Uninstaller: the components page doubles as the "delete my data?" gate —
; "Also remove my data" is an opt-IN checkbox (default OFF: keep user data).
!insertmacro MUI_UNPAGE_WELCOME
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_COMPONENTS
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_UNPAGE_FINISH

!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "German"
!insertmacro MUI_LANGUAGE "Japanese"
!insertmacro MUI_LANGUAGE "SimpChinese"

; ---------------------------------------------------------------------------
; Sections (install)
; ---------------------------------------------------------------------------

Section "Cairn (engine + tray)" SEC_CORE
  SectionIn RO                    ; required: the product itself
  SetShellVarContext current      ; per-user shells/shortcuts everywhere

  SetOutPath "$INSTDIR"
  File "${SRCDIR}\cairn.exe"
  File "${SRCDIR}\cairn-tray.exe"
  SetOutPath "$INSTDIR\bin"       ; ffmpeg lands here when the component runs

  ; ---- user PATH (idempotent; mirrors install.ps1 step 4) ----------------
  ; PowerShell does the split/compare/dedup with install.ps1's exact
  ; semantics: empty entries are dropped BEFORE the compare/join so a fresh
  ; profile (empty user PATH) never grows a leading ';'; the -notcontains
  ; compare is case-insensitive (.NET). nsExec hides its console.
  nsExec::ExecToLog `powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "$$ErrorActionPreference='Stop'; $$p=[Environment]::GetEnvironmentVariable('Path','User'); $$parts=($$p -split ';' | Where-Object {$$_ -ne ''}); if($$parts -notcontains '$INSTDIR'){[Environment]::SetEnvironmentVariable('Path',(($$parts + '$INSTDIR') -join ';'),'User')}" `
  Pop $R0
  nsExec::ExecToLog `powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "$$ErrorActionPreference='Stop'; $$p=[Environment]::GetEnvironmentVariable('Path','User'); $$parts=($$p -split ';' | Where-Object {$$_ -ne ''}); $$b='$INSTDIR\bin'; if($$parts -notcontains $$b){[Environment]::SetEnvironmentVariable('Path',(($$parts + $$b) -join ';'),'User')}" `
  Pop $R0
  ; make already-running Explorer/shells notice the new PATH
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=2000

  ; ---- first-run init (install.ps1 step 6) -------------------------------
  ; creates %USERPROFILE%\.cairn (the home store). A non-zero exit is logged
  ; but NOT fatal here: the tray supervisor retries the daemon at login, and
  ; a locked store (daemon already running from an older install) must not
  ; abort a repair install.
  nsExec::ExecToLog `"$INSTDIR\cairn.exe" init`
  Pop $R0
  ${If} $R0 != 0
    DetailPrint "cairn init exited $R0 (continuing — the tray will retry at login)"
  ${EndIf}

  ; ---- autostart (install.ps1 step 5: HKCU Run, per-user, no admin) ------
  ; Same key name AND same quoted-exe value shape as install.ps1 so the two
  ; installers are interchangeable on the same machine.
  WriteRegStr HKCU "${AUTORUN_KEY}" "${AUTORUN_VALUE}" '"$INSTDIR\cairn-tray.exe"'

  ; ---- Add/Remove Programs entry (the per-user "Apps" view) --------------
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName"     "${PRODUCT_NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion"  "${PRODUCT_VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher"       "${PRODUCT_PUBLISHER}"
  WriteRegStr HKCU "${UNINST_KEY}" "URLInfoAbout"    "${PRODUCT_URL}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon"     '"$INSTDIR\cairn-tray.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; ---- start the daemon hidden (install.ps1 step 6b parity) --------------
  Call StartCairnRuntime
SectionEnd

Section "Start Menu shortcut" SEC_SM
  SetShellVarContext current
  CreateDirectory "$SMPROGRAMS\Cairn"
  ; The tray is the product surface (connect folder, status, open console);
  ; "Open Console" reaches the dashboard/app from there (tray.rs).
  CreateShortCut "$SMPROGRAMS\Cairn\Cairn.lnk" "$INSTDIR\cairn-tray.exe" "" "$INSTDIR\cairn-tray.exe" 0 "" "" "Cairn — sync status, connect, open console"
  CreateShortCut "$SMPROGRAMS\Cairn\Uninstall Cairn.lnk" "$INSTDIR\uninstall.exe" "" "$INSTDIR\uninstall.exe" 0 "" "" "Remove Cairn"
SectionEnd

Section /o "Desktop shortcut" SEC_DT
  SetShellVarContext current
  CreateShortCut "$DESKTOP\Cairn.lnk" "$INSTDIR\cairn-tray.exe" "" "$INSTDIR\cairn-tray.exe" 0 "" "" "Cairn — sync status, connect, open console"
SectionEnd

Section "ffmpeg (editing copies + review publishing)" SEC_FFMPEG
  ; WHY ON BY DEFAULT: without ffmpeg the proxy workflow (lightweight editing
  ; copies so a remote editor pulls megabytes instead of 50 GB originals) and
  ; ffprobe-backed review publishing (auto fps/frames) are unavailable.
  ; Honest costs: ~115 MB download, ~200 MB on disk.
  DetailPrint "Downloading ffmpeg (~115 MB) from gyan.dev..."
  nsExec::ExecToLog `powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "$$ProgressPreference='SilentlyContinue'; [Net.ServicePointManager]::SecurityProtocol=[Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -Uri '${CAIRN_FFMPEG_URL}' -OutFile '$PLUGINSDIR\ffmpeg.zip'" `
  Pop $R0
  ${If} $R0 != 0
    DetailPrint "ffmpeg download failed (exit $R0)"
    Call FfmpegSkip
    Return
  ${EndIf}

  DetailPrint "Verifying ffmpeg SHA256 (fail closed on mismatch)..."
  ; The verdict travels in the EXIT CODE (no stdout whitespace traps).
  nsExec::ExecToLog `powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "$$h=(Get-FileHash -LiteralPath '$PLUGINSDIR\ffmpeg.zip' -Algorithm SHA256).Hash.Trim(); if($$h -ieq '${CAIRN_FFMPEG_SHA256}'){ exit 0 } else { Write-Host ('ffmpeg hash mismatch: ' + $$h); exit 86 }" `
  Pop $R0
  ${If} $R0 != 0
    Call FfmpegSkip
    Return
  ${EndIf}

  DetailPrint "Extracting ffmpeg.exe + ffprobe.exe into $INSTDIR\bin ..."
  ; Targeted extract (no temp tree; resolves the zip's top-level dir):
  nsExec::ExecToLog `powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "$$ErrorActionPreference='Stop'; Add-Type -AssemblyName System.IO.Compression.FileSystem; $$z=[IO.Compression.ZipFile]::OpenRead('$PLUGINSDIR\ffmpeg.zip'); foreach($$n in @('ffmpeg.exe','ffprobe.exe')){ $$e=$$z.Entries | Where-Object { $$_ -and ($$_.Name -eq $$n) } | Select-Object -First 1; if($$e){ [IO.Compression.ZipFileExtensions]::ExtractToFile($$e, '$INSTDIR\bin\' + $$n, $$true) } }; $$z.Dispose()" `
  Pop $R0
  ${If} $R0 != 0
    DetailPrint "ffmpeg extract failed (exit $R0)"
    Call FfmpegSkip
    Return
  ${EndIf}
  DetailPrint "ffmpeg installed into $INSTDIR\bin (that bin dir is on your PATH)."
SectionEnd

; ---------------------------------------------------------------------------
; Helper functions
; ---------------------------------------------------------------------------
Function FfmpegSkip
  Delete "$PLUGINSDIR\ffmpeg.zip"
  DetailPrint "ffmpeg SKIPPED — the proxy workflow and ffprobe-based review publishing will say so honestly when used."
  ${IfNot} ${Silent}
    MessageBox MB_ICONEXCLAMATION|MB_OK \
      "The optional ffmpeg download was skipped (download failure or SHA256 mismatch).$\n$\nEverything else is installed. Without ffmpeg: proxy/editing copies are unavailable and review publishing needs explicit fps/frames.$\n$\nHow to fix later: re-run this installer, or see installer/windows/README.md."
  ${EndIf}
FunctionEnd

; Start the daemon hidden (VBS via wscript — wscript is a GUI-subsystem host:
; zero console flash, no plugins) + the tray (GUI, async). The VBS starts the
; daemon windowless with stderr appended to daemon.log — the same file the
; tray supervisor (supervise.rs) and install.ps1 use. The daemon self-dedups
; on :17778 (probes before bind, exits 0 — round 27), so an already-running
; daemon is never double-spawned; the tray's supervisor restarts it on every
; login onward (ADR-0029).
Function StartCairnRuntime
  SetOutPath "$INSTDIR"
  CreateDirectory "$PROFILE\.cairn"
  FileOpen $R2 "$PLUGINSDIR\cairn_daemon_launch.vbs" w
  FileWrite $R2 `Set sh = CreateObject("WScript.Shell")$\r$\n`
  ; cmd /c with doubled outer quotes (cmd strips the outer pair); 0 = hidden window, False = do not wait
  FileWrite $R2 `sh.Run "cmd /c """"$INSTDIR\cairn.exe"" daemon 2>>""$PROFILE\.cairn\daemon.log""""", 0, False$\r$\n`
  FileClose $R2
  nsExec::ExecToLog `"$WINDIR\System32\wscript.exe" //B //Nologo "$PLUGINSDIR\cairn_daemon_launch.vbs"`
  Pop $R0
  DetailPrint "Cairn daemon started (hidden; log: %USERPROFILE%\.cairn\daemon.log)"

  ; Non-fatal on headless CI (no interactive desktop): the autostart entry
  ; covers every future login regardless.
  Exec '"$INSTDIR\cairn-tray.exe"'
FunctionEnd

Function OpenDashboard
  ${IfNot} ${Silent}
    ExecShell "open" "http://127.0.0.1:17778"
  ${EndIf}
FunctionEnd

; /NOFFMPEG switch (used by the CI gate so a release never depends on a
; third-party mirror's bandwidth): deselect the ffmpeg component silently.
Function .onInit
  ${GetOptions} $CMDLINE "/NOFFMPEG" $R9
  ${IfNot} ${Errors}
    SectionSetFlags ${SEC_FFMPEG} ${SF_OFF}
    DetailPrint "/NOFFMPEG: ffmpeg component deselected"
  ${EndIf}
FunctionEnd

; ---------------------------------------------------------------------------
; Sections (uninstall)
; ---------------------------------------------------------------------------
Section "un.Cairn program files" UNSEC_CORE
  SectionIn RO
  SetShellVarContext current

  ; Stop the runtime. CAVEAT (documented here + in the README): taskkill is a
  ; hard kill of the daemon — in-flight sync pauses; the engine is crash-safe
  ; by design (M1/M3: kill -9 at any point loses no acknowledged save; the
  ; outbox re-sends unacked work). Close your NLE first to be polite.
  DetailPrint "Stopping the Cairn daemon + tray..."
  nsExec::Exec `"$SYSDIR\taskkill.exe" /IM cairn.exe /F`
  Pop $R0
  nsExec::Exec `"$SYSDIR\taskkill.exe" /IM cairn-tray.exe /F`
  Pop $R0
  Sleep 800   ; let file handles drop before deleting the exes

  Delete "$SMPROGRAMS\Cairn\Cairn.lnk"
  Delete "$SMPROGRAMS\Cairn\Uninstall Cairn.lnk"
  RMDir  "$SMPROGRAMS\Cairn"
  Delete "$DESKTOP\Cairn.lnk"

  DeleteRegValue HKCU "${AUTORUN_KEY}" "${AUTORUN_VALUE}"
  DeleteRegKey   HKCU "${UNINST_KEY}"

  ; user PATH cleanup (PowerShell: same .NET semantics as install;
  ; case-insensitive drop of both entries we may have added)
  nsExec::ExecToLog `powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "$$ErrorActionPreference='Stop'; $$p=[Environment]::GetEnvironmentVariable('Path','User'); $$d='$INSTDIR'; $$b='$INSTDIR\bin'; $$n=($$p -split ';' | Where-Object { $$_ -and ($$_.TrimEnd('\') -ine $$d) -and ($$_.TrimEnd('\') -ine $$b) }) -join ';'; if($$n -ne $$p){[Environment]::SetEnvironmentVariable('Path',$$n,'User')}" `
  Pop $R0
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=2000

  RMDir /r "$INSTDIR"
SectionEnd

; Opt-IN (unchecked by default): %USERPROFILE%\.cairn is the Cairn home store
; (chunk CAS, journals, keys, daemon.log). Your PROJECT files in attached
; folders are ordinary files — they are NEVER touched. %LOCALAPPDATA%\cairn is
; the native window's (cairn-app) per-user data when present.
Section /o "un.Also remove my data (the .cairn store)" UNSEC_DATA
  SetShellVarContext current
  DetailPrint "Removing %USERPROFILE%\.cairn (Cairn's local store)..."
  RMDir /r "$PROFILE\.cairn"
  RMDir /r "$LOCALAPPDATA\cairn"
SectionEnd
