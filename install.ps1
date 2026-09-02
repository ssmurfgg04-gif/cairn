# Cairn -- one-command Windows installer (SHIP v1.0 Task 2).
#
#   irm https://raw.githubusercontent.com/ssmurfgg04-gif/cairn/main/install.ps1 | iex
#
# What it does:
#   1. Detects the Windows version (10/11) and edition (Pro/Home/...)
#   2. Resolves the latest GitHub release and downloads cairn-windows-*.exe
#   3. Verifies the download against the release's SHA256 asset
#   4. Adds the install dir to the user PATH (idempotent)
#   5. Runs `cairn init` (creates the store; device id is issued at `cairn login`)
#   6. Prints the next step
#
# Exit code 0 = installed; non-zero = failure (reason on stderr).
# CI gate: .github/workflows/install-windows.yml runs this on windows-latest
# on every release and asserts exit 0.
#
# For CI/testing you can pin an artifact instead of the latest release:
#   powershell -File install.ps1 -ArtifactUrl https://.../cairn-windows-v1.0.exe
[CmdletBinding()]
param(
    [string]$ArtifactUrl = "",
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
    $shaText = (Invoke-WebRequest -Uri $shaUrl -UserAgent "cairn-installer" -UseBasicParsing).Content
    $expected = ($shaText -split '\s+')[0].ToLower()
} catch {
    Fail "cannot fetch the SHA256 manifest ($shaUrl): $($_.Exception.Message)"
}
$actual = (Get-FileHash $exePath -Algorithm SHA256).Hash.ToLower()
if ($actual -ne $expected) {
    Fail "SHA256 mismatch for cairn.exe -- expected $expected, got $actual (refusing to install)"
}
Write-Host "SHA256 verified: $actual"

# Clear the Mark-of-the-Web so the freshly downloaded exe does not trip
# SmartScreen on every launch (we just verified its hash).
if (Get-Command Unblock-File -ErrorAction SilentlyContinue) {
    Unblock-File $exePath
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

# ---- 5. First-run init ------------------------------------------------------------
& $exePath init
if ($LASTEXITCODE -ne 0) {
    Fail "cairn init exited $LASTEXITCODE"
}

# ---- 6. Done ----------------------------------------------------------------------
Write-Host ""
Write-Host "Cairn installed."
Write-Host "Run 'cairn attach <folder>' to start -- the 5-minute guide walks you through it:"
Write-Host "  https://github.com/$Repo/blob/main/docs/BETA.md"
exit 0
