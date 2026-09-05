# Cairn -- clone-and-go (round 27): ONE script from zero to a running
# dashboard, written for the testing team's "clone -> autoinstall ->
# dashboard" retro. Handles every pothole they hit:
#
#   * repo pre-exists -> git pull (not a broken fresh clone), diverged
#     branches -> reset --hard origin/main (their `fatal: Divergent
#     branches can't be fast-forwarded`)
#   * toolchain mismatch -> rustup installs the pinned 1.98.0
#     automatically (their host had 1.97.1)
#   * build takes ~10 min -> a RELEASE download is tried FIRST when the
#     repo tag matches the latest GitHub release; local build is the
#     fallback (and for dev iterations)
#   * running daemon/tray hold the exe -> stopped first
#   * install.ps1 handles the rest (auth'd API retries, atomic
#     download, SHA, PATH dedup, autostart, init, single daemon, tray)
#   * at the end: dashboard opens in the browser, cairn-app launches
#     when present
#
# Usage (PowerShell):
#   irm https://raw.githubusercontent.com/ssmurfgg04-gif/cairn/main/clone-and-go.ps1 | iex
# or from a checkout:
#   powershell -File clone-and-go.ps1 [-RepoDir .\cairn] [-ForceBuild]
#
# Env:
#   GH_TOKEN / GITHUB_TOKEN  - a classic PAT; lifts the 60/hr anonymous
#                               API ceiling the retro's 403s hit
#   CAIRN_INSTALL_NO_LAUNCH=1 - CI shape: install only, start nothing
[CmdletBinding()]
param(
    [string]$RepoDir = "$env:USERPROFILE\cairn",
    [string]$Repo = "ssmurfgg04-gif/cairn",
    [switch]$ForceBuild
)

$ErrorActionPreference = "Stop"
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch { }

function Fail {
    param([string]$Message)
    [Console]::Error.WriteLine("clone-and-go.ps1: $Message")
    exit 1
}

Write-Host "== cairn clone-and-go ==" -ForegroundColor Cyan

# ---- 1. repo: clone when missing, pull (or realign) when present -------
if (Test-Path (Join-Path $RepoDir ".git")) {
    Write-Host "repo exists at $RepoDir - updating"
    Push-Location $RepoDir
    try {
        git fetch origin --quiet
        # divergent local history cannot fast-forward: realign to origin
        # (the retro's `809d17c..8a48ba7 57e6c27..15ac76d` shape)
        git reset --hard origin/main --quiet
        git clean -fdq  # drop stale build artifacts (they invalidate rust-cache keys)
    } finally {
        Pop-Location
    }
} elseif (Test-Path $RepoDir) {
    Fail "$RepoDir exists but is not a git checkout - move it aside and re-run"
} else {
    Write-Host "cloning $Repo into $RepoDir"
    git clone --depth 1 "https://github.com/$Repo.git" $RepoDir
    if ($LASTEXITCODE -ne 0) { Fail "git clone failed" }
}

$InstallPs1 = Join-Path $RepoDir "install.ps1"

# ---- 2. Prefer the published release; build locally as the fallback ----
# The release path needs no toolchain and takes ~1 min. The local build
# needs rustup + ~10 min (the retro's 9m16s release build) and only wins
# when the checkout carries uncommitted changes.
$useRelease = $false
if (-not $ForceBuild) {
    try {
        $api = "https://api.github.com/repos/$Repo/releases/latest"
        $tok = [Environment]::GetEnvironmentVariable("GH_TOKEN")
        if (-not $tok) { $tok = [Environment]::GetEnvironmentVariable("GITHUB_TOKEN") }
        $rel = $null
        if ($tok) {
            $rel = Invoke-RestMethod -Uri $api -UserAgent "cairn-installer" -Headers @{ Authorization = "token $tok" } -UseBasicParsing
        } else {
            $rel = Invoke-RestMethod -Uri $api -UserAgent "cairn-installer" -UseBasicParsing
        }
        $asset = $rel.assets | Where-Object { $_.name -match '^cairn-windows-.*\.exe$' } | Select-Object -First 1
        if ($asset) {
            Write-Host "release $($rel.tag_name) found - installing binaries (no toolchain needed)"
            $useRelease = $true
            & powershell -NoProfile -ExecutionPolicy Bypass -File $InstallPs1 -ArtifactUrl $asset.browser_download_url
            if ($LASTEXITCODE -ne 0) {
                Write-Host "release install failed - falling back to a local build" -ForegroundColor Yellow
                $useRelease = $false
            }
        }
    } catch {
        Write-Host "no release reachable ($($_.Exception.Message)) - building locally" -ForegroundColor Yellow
        $useRelease = $false
    }
}

if (-not $useRelease) {
    # ---- 2b. local build: ensure the pinned toolchain --------------------
    $pinned = (Get-Content (Join-Path $RepoDir "rust-toolchain.toml") | Select-String 'channel = "([\d.]+)"').Matches[0].Groups[1].Value
    $have = ""
    try { $have = (rustc --version) -replace 'rustc (\S+).*', '$1' } catch { }
    if (-not $have) {
        Write-Host "rustup not found - installing toolchain $pinned"
        Invoke-RestMethod -Uri "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe" -OutFile "$env:TEMP\rustup-init.exe" -UserAgent "cairn-installer"
        & "$env:TEMP\rustup-init.exe" -y --default-toolchain $pinned --profile minimal -c clippy,rustfmt | Out-Null
        $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
    } elseif ($have -ne $pinned) {
        Write-Host "host toolchain $have != pinned $pinned - installing $pinned"
        rustup toolchain install $pinned --profile minimal | Out-Null
    }

    Push-Location $RepoDir
    try {
        # --message-format short: the retro hit tool timeouts watching the
        # long progress spam; short lines keep the console quiet
        Write-Host "building cairn (release) - ~10 min on a typical box"
        cargo build -p cairn-cli --release --message-format short
        if ($LASTEXITCODE -ne 0) { Fail "cargo build failed" }
    } finally {
        Pop-Location
    }

    $builtExe = Join-Path $RepoDir "target\release\cairn.exe"
    if (-not (Test-Path $builtExe)) { Fail "built cairn.exe not found at $builtExe" }

    # install the locally built binary: install.ps1's local-artifact path
    # copies it in (no .sha256 sidecar, no web fetch) and still handles
    # PATH dedup + autostart + init + single-daemon + tray
    & powershell -NoProfile -ExecutionPolicy Bypass -File $InstallPs1 -ArtifactUrl $builtExe
    if ($LASTEXITCODE -ne 0) { Fail "install.ps1 failed for the local build" }
}

# ---- 3. open the dashboard + the native window when present -----------
if ($env:CAIRN_INSTALL_NO_LAUNCH -ne "1") {
    $exePath = "$env:LOCALAPPDATA\Programs\cairn\cairn.exe"
    # wait for the daemon's dashboard to answer before pointing a browser at it
    $deadline = (Get-Date).AddSeconds(30)
    $up = $false
    while ((Get-Date) -lt $deadline) {
        try {
            $resp = Invoke-WebRequest -Uri "http://127.0.0.1:17778" -UseBasicParsing -TimeoutSec 2
            if ($resp.StatusCode -eq 200) { $up = $true; break }
        } catch { Start-Sleep -Milliseconds 800 }
    }
    if ($up) {
        Start-Process "http://127.0.0.1:17778"
        Write-Host "dashboard open in your browser: http://127.0.0.1:17778"
    } else {
        Write-Host "dashboard not answering yet - open http://127.0.0.1:17778 in a minute, or run 'cairn daemon'"
    }
    $appExe = "$env:LOCALAPPDATA\cairn\cairn-app.exe"
    if (Test-Path $appExe) {
        Start-Process $appExe
        Write-Host "cairn-app launched"
    }
}

Write-Host ""
Write-Host "done - the tray icon (near the clock) supervises the daemon from every login."
Write-Host "next: click 'Add to Workspace' on the dashboard and pick a project folder."
exit 0
