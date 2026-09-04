# Cairn -- one-command Windows installer (round 12: CLI + tray, ADR-0016;
# round 19: + the cairn-app native window, ADR-0022).
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
#   9. Prints the next step
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
        $rel = Invoke-RestMethod -Uri $api -UserAgent "cairn-installer" -UseBasicParsing
    } catch {
        Fail "cannot query the latest release ($api): $($_.Exception.Message)"
    }
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
$shaUrl = "$exeUrl.sha256"

# ---- 3. Download + verify ---------------------------------------------------------
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$exePath = Join-Path $InstallDir "cairn.exe"

Invoke-WebRequest -Uri $exeUrl -OutFile $exePath -UserAgent "cairn-installer" -UseBasicParsing
if (-not (Test-Path $exePath)) { Fail "download failed: $exeUrl" }

$expected = $null
try {
    # PS 5.1 returns [byte[]] for application/octet-stream (all release assets);
    # PS 7 returns a string. Decode bytes explicitly so -split parses the manifest,
    # not the byte-array's decimal dump.
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
$actual = (Get-FileHash $exePath -Algorithm SHA256).Hash.ToLower()
if ($actual -ne $expected) {
    Fail "SHA256 mismatch for cairn.exe -- expected $expected, got $actual (refusing to install)"
}
Write-Host "SHA256 verified: $actual"

# ---- 3b. Download + verify the TRAY (optional asset: older releases, dev
# pins and partial releases may ship engine-only; installer degrades
# gracefully and says so) -------------------------------------------
$trayUrl = "$exeUrl".Replace("cairn-windows-", "cairn-tray-windows-")
$trayPath = Join-Path $InstallDir "cairn-tray.exe"
$trayInstalled = $false
try {
    Invoke-WebRequest -Uri $trayUrl -OutFile $trayPath -UserAgent "cairn-installer" -UseBasicParsing
    $trayShaUrl = "$trayUrl.sha256"
    $tresp = Invoke-WebRequest -Uri $trayShaUrl -UserAgent "cairn-installer" -UseBasicParsing
    $tshaText = if ($tresp.Content -is [byte[]]) {
        [Text.Encoding]::ASCII.GetString($tresp.Content)
    } else {
        $tresp.Content
    }
    $texpected = ($tshaText.Trim() -split '\s+')[0].ToLower()
    $tactual = (Get-FileHash $trayPath -Algorithm SHA256).Hash.ToLower()
    if ($tactual -ne $texpected) {
        Remove-Item $trayPath -Force
        Write-Host "tray SHA256 mismatch -- skipping tray (engine still fully installed)"
    } else {
        if (Get-Command Unblock-File -ErrorAction SilentlyContinue) {
            Unblock-File $trayPath
        }
        $trayInstalled = $true
        Write-Host "cairn-tray.exe installed (SHA256 verified: $tactual)"
    }
} catch {
    if (Test-Path $trayPath) { Remove-Item $trayPath -Force }
    Write-Host "no tray asset on this release -- engine-only install (the CLI path works; re-run after a release that ships the tray)"
}

# Clear the Mark-of-the-Web so the freshly downloaded exe does not trip
# SmartScreen on every launch (we just verified its hash).
if (Get-Command Unblock-File -ErrorAction SilentlyContinue) {
    Unblock-File $exePath
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
        Invoke-WebRequest -Uri $appUrl -OutFile $setupPath -UserAgent "cairn-installer" -UseBasicParsing
        $appShaUrl = "$appUrl.sha256"
        $aresp = Invoke-WebRequest -Uri $appShaUrl -UserAgent "cairn-installer" -UseBasicParsing
        $ashaText = if ($aresp.Content -is [byte[]]) {
            [Text.Encoding]::ASCII.GetString($aresp.Content)
        } else {
            $aresp.Content
        }
        $aexpected = ($ashaText.Trim() -split '\s+')[0].ToLower()
        $aactual = (Get-FileHash $setupPath -Algorithm SHA256).Hash.ToLower()
        if ($aactual -ne $aexpected) {
            Remove-Item $setupPath -Force
            Write-Host "window bundle SHA256 mismatch -- skipping cairn-app (browser console still fully installed)"
        } else {
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

# ---- 4. PATH (user scope, idempotent) ---------------------------------------------
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($userPath -split ';') -notcontains $InstallDir) {
    $newPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    Write-Host "Added $InstallDir to your user PATH (new shells pick it up)"
}
# Make the binary visible to THIS session too (the registry write above does not
# propagate into an already-running process).
if (($env:Path -split ';') -notcontains $InstallDir) {
    $env:Path = "$env:Path;$InstallDir"
}

# ---- 5. Tray autostart + shortcut + launch (only when the tray installed) ----------
if ($trayInstalled) {
    # HKCU Run key: per-user autostart, NO admin rights, silent on boot.
    # Idempotent: overwrite with the same value.
    $runKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
    try {
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
