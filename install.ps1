# Cairn -- one-command Windows installer (round 12: CLI + tray, ADR-0016;
# round 19: + the cairn-app native window, ADR-0022; round 27: the
# install retro hardening).
#
#   irm https://raw.githubusercontent.com/ssmurfgg04-gif/cairn/main/install.ps1 | iex
#
# What it does:
#   1. Detects the Windows version (10/11) and edition (Pro/Home/...)
#   2. Resolves the latest GitHub release and downloads:
#        - cairn-windows-*.exe    (the engine: CLI + daemon + server)
#        - cairn-tray-windows-*.exe (the system tray, ADR-0016 "clicky-clicky")
#        - cairn-window-*-setup.exe (the NSIS bundle: the native console
#          window, ADR-0022 -- optional, degrades to the browser console)
#   3. Verifies each download against the release's SHA256 assets
#   4. Adds the install dir to the user PATH (idempotent)
#   5. Registers the tray to start at login (HKCU Run key, per-user, no admin)
#      and creates a Desktop shortcut to it
#   6. Runs the NSIS bundle silently (/S, per-user) so cairn-app.exe lands
#      beside the engine + tray -- the tray's "Open Console" finds it there
#   7. Starts the tray for THIS session (no reboot needed to see it)
#   8. Runs `cairn init` (creates the store; device id is issued at `cairn login`)
#   9. Starts the daemon hidden (stderr -> <home>/daemon.log) ONLY when no
#      daemon already answers on :17778 -- round 27: the installer + tray
#      supervisor + user's own `cairn daemon` all racing the port produced
#      "10048 Only one usage of each socket address" in every daemon.log.
#      The daemon now self-dedups too (probes before bind, exits 0).
#      (round 26 kept: CAIRN_INSTALL_NO_LAUNCH skips this)
#  10. Prints the next step
#
# Round 27 hardening (the clone-to-dashboard retro):
#   * GitHub API 403 (anonymous 60/hr): the API call carries
#     Authorization from $env:GH_TOKEN / $env:GITHUB_TOKEN when present,
#     retries 4x with backoff, and falls back to parsing the
#     /releases/latest redirect for the tag when the API stays closed.
#     Asset downloads themselves need no auth.
#   * File-in-use (cairn.exe locked by the running daemon/tray):
#     Stop-Process cairn,cairn-tray BEFORE any download, wait for the
#     handles to drop, download to a .tmp file, Move-Item -Force into
#     place, hash AFTER the move (a truncated in-flight file used to
#     compare CDDB3AC3... vs 1536FEAF...).
#   * The PATH write dedups both registry and session scope.
#
# Explorer badge registration is NOT installer work: the daemon registers the
# CfAPI sync root + provider state at `cairn attach` time (badge.rs). The
# installer only needs the binaries + autostart.
#
# Exit code 0 = installed; non-zero = failure (reason on stderr).
# CI gate: .github/workflows/release.yml installer-gate runs this on
# windows-latest against the just-published release and asserts exit 0.
#
# For CI/testing you can pin an artifact instead of the latest release:
#   powershell -File install.ps1 -ArtifactUrl https://.../cairn-windows-v1.0.exe
[CmdletBinding()]
param(
    [string]$ArtifactUrl = "",
    [string]$AppSetupUrl = "",
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\cairn",
    [string]$Repo = "ssmurfgg04-gif/cairn"
)

$ErrorActionPreference = "Stop"
# Windows PowerShell 5.1 defaults can leave TLS 1.2 off; Win10/11 servers speak it.
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch { }

function Fail {
    param([string]$Message)
    [Console]::Error.WriteLine("install.ps1: $Message")
    exit 1
}

# ---- helpers: retrying web calls + atomic replace ----------------------

# An auth header for the GitHub API ONLY when a token is available (the
# asset browser_download_url works anonymously; the 60/hr API ceiling is
# what 403'd the retro). Never echoed - tokens are credentials.
function Get-GhApiHeaders {
    $tok = $null
    foreach ($name in @("GH_TOKEN", "GITHUB_TOKEN")) {
        $v = [Environment]::GetEnvironmentVariable($name)
        if ($v) { $tok = $v; break }
    }
    if ($tok) { return @{ Authorization = "token $tok" } }
    return @{}
}

function Invoke-RestWithRetry {
    param([string]$Uri, [int]$Tries = 4)
    $delay = 2
    for ($i = 1; $i -le $Tries; $i++) {
        try {
            $h = Get-GhApiHeaders
            if ($h.Count -gt 0) {
                return Invoke-RestMethod -Uri $Uri -UserAgent "cairn-installer" -Headers $h -UseBasicParsing
            }
            return Invoke-RestMethod -Uri $Uri -UserAgent "cairn-installer" -UseBasicParsing
        } catch {
            $resp = $_.Exception.Response
            $code = if ($resp) { [int]$resp.StatusCode } else { 0 }
            if ($i -eq $Tries) { throw }
            if ($code -eq 403 -or $code -eq 429 -or $code -eq 0) {
                Write-Host "api attempt $i failed (HTTP $code) - retrying in ${delay}s (set GH_TOKEN to lift the rate limit)"
                Start-Sleep -Seconds $delay
                $delay = [Math]::Min($delay * 2, 30)
            } else {
                throw
            }
        }
    }
    throw "unreachable"
}

# Download to <dest>.tmp, verify the SHA, THEN move into place: a
# half-written file can never be mistaken for an install, and the
# file-in-use window (daemon still holding the old exe) is closed by
# Stop-Cairn before we get here.
function Install-VerifiedExe {
    param([string]$Url, [string]$Dest, [string]$What)
    $tmp = "$Dest.tmp"
    if (Test-Path $tmp) { Remove-Item $tmp -Force -ErrorAction SilentlyContinue }
    $shaUrl = "$Url.sha256"
    $expected = $null
    try {
        $resp = Invoke-WebRequest -Uri $shaUrl -UserAgent "cairn-installer" -UseBasicParsing
        $shaText = if ($resp.Content -is [byte[]]) {
            [Text.Encoding]::ASCII.GetString($resp.Content)
        } else {
            $resp.Content
        }
        $expected = ($shaText.Trim() -split '\s+')[0].ToLower()
    } catch {
        Fail "cannot fetch the SHA256 manifest ($shaUrl): $($_.Exception.Message)"
    }
    try {
        Invoke-WebRequest -Uri $Url -OutFile $tmp -UserAgent "cairn-installer" -UseBasicParsing
    } catch {
        if (Test-Path $tmp) { Remove-Item $tmp -Force }
        Fail "download failed: $Url ($($_.Exception.Message))"
    }
    if (-not (Test-Path $tmp)) { Fail "download produced no file: $Url" }
    $actual = (Get-FileHash $tmp -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) {
        Remove-Item $tmp -Force
        Fail "SHA256 mismatch for $What -- expected $expected, got $actual (refusing to install)"
    }
    Move-Item -Path $tmp -Destination $Dest -Force
    if (Get-Command Unblock-File -ErrorAction SilentlyContinue) {
        Unblock-File $Dest
    }
    Write-Host "$What installed (SHA256 verified: $actual)"
}

# Stop the running daemon + tray so the exes we are about to replace are
# not held open (retro error #2: "being used by another process" ->
# truncated download -> hash mismatch).
function Stop-Cairn {
    foreach ($n in @("cairn", "cairn-tray", "cairn-app")) {
        Get-Process -Name $n -ErrorAction SilentlyContinue | ForEach-Object {
            Write-Host "stopping $($_.ProcessName) (pid $($_.Id)) so the update can replace it"
            Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
        }
    }
    # handles take a beat to drop after kill
    $deadline = (Get-Date).AddSeconds(10)
    while ((Get-Date) -lt $deadline) {
        $alive = Get-Process -Name cairn,cairn-tray -ErrorAction SilentlyContinue
        if (-not $alive) { break }
        Start-Sleep -Milliseconds 300
    }
}

# Is a daemon already answering on the dashboard port? (round 27: the
# double-start fix - the installer must not spawn a second daemon.)
function Test-DaemonUp {
    try {
        $c = New-Object Net.Sockets.TcpClient
        $r = $c.BeginConnect("127.0.0.1", 17778, $null, $null)
        $ok = $r.AsyncWaitHandle.WaitOne(500)
        if ($ok -and $c.Connected) { $c.Close(); return $true }
        $c.Close()
        return $false
    } catch { return $false }
}

# ---- 1. Windows version + edition -------------------------------------------------
$cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
$build = [int]$cv.CurrentBuildNumber
if ($build -lt 10240) {
    Fail "Cairn requires Windows 10 or newer (found build $build)"
}
$winName = if ($build -ge 22000) { "Windows 11" } else { "Windows 10" }
Write-Host "Detected: $winName ($($cv.ProductName), build $build)"

# ---- 2. Resolve the release asset -------------------------------------------------
$release = $null
if ($ArtifactUrl -ne "") {
    $exeUrl = $ArtifactUrl
    Write-Host "Using pinned artifact: $exeUrl"
} else {
    $api = "https://api.github.com/repos/$Repo/releases/latest"
    $rel = $null
    try {
        $rel = Invoke-RestWithRetry -Uri $api
    } catch {
        # API closed (persistent 403 with no token): fall back to the
        # releases/latest REDIRECT - GitHub answers it anonymously and
        # lands on /tag/<tag>, which names the assets deterministically.
        Write-Host "API path exhausted - trying the tag redirect fallback"
        try {
            $tagReq = [System.Net.HttpWebRequest]::Create("https://github.com/$Repo/releases/latest")
            $tagReq.AllowAutoRedirect = $false
            $tagReq.UserAgent = "cairn-installer"
            $tagResp = $tagReq.GetResponse()
            $loc = $tagResp.Headers["Location"]
            $tagResp.Close()
            $tag = ($loc -split '/tag/')[-1]
            if (-not $tag) { throw "no tag in redirect" }
            $exeUrl = "https://github.com/$Repo/releases/download/$tag/cairn-windows-$tag.exe"
            $rel = $null
            Write-Host "Fallback tag resolved: $tag"
        } catch {
            Fail "cannot resolve the latest release: $($_.Exception.Message) -- set GH_TOKEN to lift the API rate limit, or re-run in ~an hour"
        }
    }
    if ($rel) {
        $release = $rel
        $asset = $rel.assets |
            Where-Object { $_.name -match '^cairn-windows-.*\.exe$' } |
            Select-Object -First 1
        if (-not $asset) {
            Fail "no cairn-windows-*.exe asset on release $($rel.tag_name) -- was the release workflow run?"
        }
        $exeUrl = $asset.browser_download_url
        Write-Host "Latest release: $($rel.tag_name) -- asset $($asset.name)"
    }
}

# ---- 3. Stop what holds the exes, then download + verify atomically ---------------
Stop-Cairn
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$exePath = Join-Path $InstallDir "cairn.exe"
# A LOCAL artifact (clone-and-go's local-build path, or a dev pin) is
# copied in, not downloaded: no .sha256 sidecar exists and
# Invoke-WebRequest cannot read a bare filesystem path. The local build
# is trusted (it never left the machine).
if ($ArtifactUrl -ne "" -and (Test-Path $ArtifactUrl)) {
    Copy-Item $ArtifactUrl $exePath -Force
    Write-Host "cairn.exe installed from local artifact: $ArtifactUrl"
} else {
    Install-VerifiedExe -Url $exeUrl -Dest $exePath -What "cairn.exe"
}

# ---- 3b. Download + verify the TRAY (optional asset: older releases, dev
# pins and partial releases may ship engine-only; installer degrades
# gracefully and says so) -------------------------------------------
$trayPath = Join-Path $InstallDir "cairn-tray.exe"
$trayInstalled = $false
$localTray = ""
$localArtifact = ($ArtifactUrl -ne "" -and (Test-Path $ArtifactUrl))
if ($localArtifact) {
    # local build: the tray sits beside the engine in the same target dir
    $localTray = Join-Path (Split-Path $ArtifactUrl -Parent) "cairn-tray.exe"
}
if ($localTray -ne "" -and (Test-Path $localTray)) {
    Copy-Item $localTray $trayPath -Force
    $trayInstalled = $true
    Write-Host "cairn-tray.exe installed from local artifact"
} elseif ($localArtifact) {
    # a local artifact with no tray sibling is an engine-only build
    # (cargo build -p cairn-cli): do NOT try to web-fetch a derived
    # local path — the beta installer gate hit exactly that
    Write-Host "no tray beside the local artifact -- engine-only install (build cairn-tray for the tray)"
} else {
    $trayUrl = "$exeUrl".Replace("cairn-windows-", "cairn-tray-windows-")
    try {
        Install-VerifiedExe -Url $trayUrl -Dest $trayPath -What "cairn-tray.exe"
        $trayInstalled = $true
    } catch {
        if (Test-Path $trayPath) { Remove-Item $trayPath -Force }
        Write-Host "no tray asset on this release -- engine-only install (the CLI path works; re-run after a release that ships the tray)"
    }
}

# ---- 3c. Download + verify + RUN the WINDOW bundle (optional asset: the
# NSIS setup for cairn-app, ADR-0022; older releases ship without it and
# the browser console carries on -- degrade loudly, never half-install) ---
$appInstalled = $false
if ($AppSetupUrl -ne "") {
    $appUrl = $AppSetupUrl
} elseif ($release -and $release.assets) {
    $appAsset = $release.assets |
        Where-Object { $_.name -match '^cairn-window-.*-setup\.exe$' } |
        Select-Object -First 1
    $appUrl = if ($appAsset) { $appAsset.browser_download_url } else { "" }
} else {
    $appUrl = ""
}
if ($appUrl -ne "") {
    $setupPath = Join-Path $env:TEMP "cairn-window-setup.exe"
    try {
        $tmpSetup = "$setupPath.tmp"
        Invoke-WebRequest -Uri $appUrl -OutFile $tmpSetup -UserAgent "cairn-installer" -UseBasicParsing
        $appShaUrl = "$appUrl.sha256"
        $aresp = Invoke-WebRequest -Uri $appShaUrl -UserAgent "cairn-installer" -UseBasicParsing
        $ashaText = if ($aresp.Content -is [byte[]]) {
            [Text.Encoding]::ASCII.GetString($aresp.Content)
        } else {
            $aresp.Content
        }
        $aexpected = ($ashaText.Trim() -split '\s+')[0].ToLower()
        $aactual = (Get-FileHash $tmpSetup -Algorithm SHA256).Hash.ToLower()
        if ($aactual -ne $aexpected) {
            Remove-Item $tmpSetup -Force -ErrorAction SilentlyContinue
            Write-Host "window bundle SHA256 mismatch -- skipping cairn-app (browser console still fully installed)"
        } else {
            Move-Item -Path $tmpSetup -Destination $setupPath -Force
            if (Get-Command Unblock-File -ErrorAction SilentlyContinue) {
                Unblock-File $setupPath
            }
            Write-Host "cairn-window-setup.exe verified (SHA256: $aactual) -- installing (per-user, silent)"
            # /S = NSIS silent; per-user installMode (tauri.conf) lands
            # cairn-app.exe in the standard LOCALAPPDATA spot -- the tray's
            # "Open Console" looks there and beside itself.
            $proc = Start-Process -FilePath $setupPath -ArgumentList "/S" -Wait -PassThru
            if ($proc.ExitCode -eq 0) {
                $appInstalled = $true
            } else {
                Write-Host "window bundle installer exited $($proc.ExitCode) -- the browser console remains the fallback"
            }
            Remove-Item $setupPath -Force -ErrorAction SilentlyContinue
        }
    } catch {
        if (Test-Path $setupPath) { Remove-Item $setupPath -Force -ErrorAction SilentlyContinue }
        Write-Host "window bundle unavailable ($($_.Exception.Message)) -- browser console it is"
    }
} else {
    Write-Host "no cairn-window-*-setup.exe asset on this release -- the console opens in your browser (same surface)"
}

# ---- 4. PATH (user scope, idempotent, DEDUPED both scopes) ------------------------
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not $userPath) { $userPath = "" }
$userParts = @($userPath -split ';' | Where-Object { $_ -ne "" })
if ($userParts -notcontains $InstallDir) {
    $userParts += $InstallDir
    [Environment]::SetEnvironmentVariable("Path", ($userParts -join ";"), "User")
    Write-Host "Added $InstallDir to your user PATH (new shells pick it up)"
}
# Make the binary visible to THIS session too (the registry write above does not
# propagate into an already-running process).
$sessionParts = @($env:Path -split ';' | Where-Object { $_ -ne "" })
if ($sessionParts -notcontains $InstallDir) {
    $env:Path = "$env:Path;$InstallDir"
}

# ---- 5. Tray autostart + shortcut + launch (only when the tray installed) ----------
if ($trayInstalled) {
    # HKCU Run key: per-user autostart, NO admin rights, silent on boot.
    # Idempotent: overwrite with the same value.
    # NOTE: on a FRESH user profile the Run key does not exist yet (the CI
    # runner profile is exactly this shape) -- Set-ItemProperty cannot write
    # into a missing key, so create it first. New-Item -Force builds the whole
    # path. Without this, a first install silently loses tray autostart.
    $runKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
    try {
        if (-not (Test-Path $runKey)) {
            New-Item -Path $runKey -Force | Out-Null
            Write-Host "Created the HKCU Run key (fresh profile)"
        }
        Set-ItemProperty -Path $runKey -Name "CairnTray" -Value "`"$trayPath`"" -ErrorAction Stop
        Write-Host "Tray autostart registered (HKCU Run)"
    } catch {
        Write-Host "could not write the autostart key: $($_.Exception.Message) -- start the tray manually"
    }
    # Desktop shortcut to the tray (the visible affordance).
    try {
        $desktop = [Environment]::GetFolderPath("Desktop")
        $lnk = Join-Path $desktop "Cairn.lnk"
        $shell = New-Object -ComObject WScript.Shell
        $sc = $shell.CreateShortcut($lnk)
        $sc.TargetPath = $trayPath
        $sc.WorkingDirectory = $InstallDir
        $sc.Description = "Cairn -- sync status, connect, open project folder"
        $sc.Save()
        Write-Host "Desktop shortcut created: $lnk"
    } catch {
        Write-Host "could not create the desktop shortcut: $($_.Exception.Message)"
    }
    # Start it NOW (the user should not need a reboot to see the tray).
    # CI sets CAIRN_INSTALL_NO_LAUNCH=1 (no interactive session there).
    if ($env:CAIRN_INSTALL_NO_LAUNCH -ne "1") {
        try {
            Start-Process -FilePath $trayPath -WorkingDirectory $InstallDir
            Write-Host "Cairn tray started"
        } catch {
            Write-Host "could not start the tray: $($_.Exception.Message) -- launch it from the shortcut"
        }
    }
}

# ---- 6. First-run init ------------------------------------------------------------
& $exePath init
if ($LASTEXITCODE -ne 0) {
    Fail "cairn init exited $LASTEXITCODE"
}

# ---- 6b. Start the daemon NOW (round 26: install-and-it-just-works;
# round 27: ONLY when one is not already up - the 10048 dedup) -----------
# The tray supervises it from every login onward (supervise.rs: probe ->
# spawn hidden -> backoff; the daemon itself self-dedups on the bind).
# CI (CAIRN_INSTALL_NO_LAUNCH=1) skips it.
if ($env:CAIRN_INSTALL_NO_LAUNCH -ne "1") {
    if (Test-DaemonUp) {
        Write-Host "Cairn daemon already running on :17778 - leaving it alone (single owner)."
    } else {
        try {
            $cairnHome = if ($env:CAIRN_HOME) { $env:CAIRN_HOME } else { Join-Path $env:USERPROFILE ".cairn" }
            if (-not (Test-Path $cairnHome)) { New-Item -ItemType Directory -Force -Path $cairnHome | Out-Null }
            $daemonLog = Join-Path $cairnHome "daemon.log"
            Start-Process -FilePath $exePath -ArgumentList "daemon" -WindowStyle Hidden -RedirectStandardError $daemonLog | Out-Null
            Write-Host "Cairn daemon started (hidden)."
        } catch {
            Write-Host "could not start the daemon: $($_.Exception.Message) -- the tray restarts it at next login, or run 'cairn daemon'"
        }
    }
    Write-Host "Dashboard: http://127.0.0.1:17778  (open it in your browser)"
}

# ---- 7. Done ----------------------------------------------------------------------
Write-Host ""
if ($trayInstalled) {
    Write-Host "Cairn installed."
    Write-Host "The tray icon is in your notification area: right-click it to connect a project folder,"
    Write-Host "check status, or open the console -- no terminal needed."
    if ($appInstalled) {
        Write-Host "The native console window (cairn-app) is installed -- tray > Open Console launches it."
    }
} else {
    Write-Host "Cairn installed (engine only -- no tray on this release)."
    Write-Host "Run 'cairn attach <folder>' to start -- the 5-minute guide walks you through it:"
    Write-Host "  https://github.com/$Repo/blob/main/docs/BETA.md"
}
Write-Host "Guide + beta docs: https://github.com/$Repo/blob/main/docs/BETA.md"
exit 0
