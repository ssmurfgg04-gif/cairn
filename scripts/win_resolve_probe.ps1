# win_resolve_probe.ps1 -- DaVinci Resolve-on-a-runner probe (Round 13, part 2).
#
# The user's ask: "the github runners have windows vms or install davinci etc
# light versions on it". This probe answers it EMPIRICALLY and honestly: it
# attempts the full ladder (search -> silent install -> launch -> record) on
# a real GitHub Actions windows runner and writes whatever happened into one
# JSON. It is a PROBE, not a gate: every outcome (including "Resolve refuses
# to start on a GPU-less runner") is DATA -- that answer then decides whether
# the H4/H5 rows can ever run on CI or stay studio-legs.
#
# Pure ASCII (PS 5.1 reads BOM-less ps1 as ANSI cp1252; Round 10 lesson).
#
# Usage:
#   powershell -File scripts\win_resolve_probe.ps1 [-Out path.json]
# Exit code: 0 always (the JSON carries the verdict; a broken probe
# machinery itself exits 1).

param(
    [string]$Out = "",
    [int]$InstallTimeoutSec = 1500,   # 25 min: ~3 GB download + installer
    [int]$LaunchTimeoutSec = 150
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
if (-not $Out) { $Out = Join-Path $repo "docs\nle-matrix-results\resolve-probe.json" }

$r = [ordered]@{
    schema = "cairn-nle-matrix/resolve-probe/1"
    captured_utc = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    host_os = "Windows " + [System.Environment]::OSVersion.Version.ToString()
    steps = [ordered]@{}
}

function Step {
    # run one probe step inside its own trap; record ok + detail; NEVER abort
    param([string]$Name, [scriptblock]$Body)
    try {
        $v = & $Body
        $r.steps[$Name] = [ordered]@{ ok = $true; detail = [string]$v }
        Write-Output ("PROBE {0}: OK -- {1}" -f $Name, $v)
    } catch {
        $r.steps[$Name] = [ordered]@{ ok = $false; detail = $_.Exception.Message }
        Write-Output ("PROBE {0}: FAIL -- {1}" -f $Name, $_.Exception.Message)
    }
}

$freeGb = [math]::Round((Get-PSDrive C).Free / 1GB, 1)
$r.steps["disk"] = [ordered]@{ ok = $true; detail = ("C: free {0} GB" -f $freeGb) }
Write-Output ("PROBE disk: C: free {0} GB" -f $freeGb)

# (1) what does winget actually know? (records the package-id answer, which
#     drifts; the probe must adapt rather than assume)
$found = ""
Step "winget-search" {
    # native stderr + EAP=Stop + 2>&1 = the PS 5.1 ErrorRecord trap; soften
    # locally so winget's stderr chatter cannot abort the probe
    $prev = $ErrorActionPreference; $ErrorActionPreference = "Continue"
    try {
        $out = & winget search "DaVinci Resolve" --accept-source-agreements 2>&1 |
            Out-String
    } finally { $ErrorActionPreference = $prev }
    $script:found = $out
    if ($out -match "BlackmagicDesign\.DaVinciResolve\S*") {
        "package id(s) present: " + $Matches[0]
    } else {
        throw ("no BlackmagicDesign DaVinciResolve id in search output: " +
            ($out -replace "`r?`n", " | "))
    }
}
$pkgId = "BlackmagicDesign.DaVinciResolve"
if ($found -match "BlackmagicDesign\.DaVinciResolveStudio") {
    # free version is the one without the Studio suffix; keep the pinned id
}

# (2) silent install (the free version's EULA is accepted via winget flags;
#     hard timeout so a hung installer cannot eat the job)
$installed = $false
Step "winget-install" {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = "winget"
    $cmdArgs = @("install", "--id", $pkgId, "-e", "--silent",
                 "--accept-package-agreements", "--accept-source-agreements")
    if ($psi.PSObject.Properties["ArgumentList"]) {
        foreach ($a in $cmdArgs) { [void]$psi.ArgumentList.Add($a) }
    } else {
        $psi.Arguments = ($cmdArgs -join " ")
    }
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    if (-not $p.WaitForExit($InstallTimeoutSec * 1000)) {
        try { $p.Kill() } catch { }
        throw ("winget install timed out after {0} s" -f $InstallTimeoutSec)
    }
    $out = $p.StandardOutput.ReadToEnd() + $p.StandardError.ReadToEnd()
    # winget exit 0x8A15002B = "already installed" (fine for us)
    if ($p.ExitCode -ne 0 -and $p.ExitCode -ne -1978335133) {
        throw ("winget exit {0}: {1}" -f $p.ExitCode,
            ($out -replace "`r?`n", " | ").Substring(0, [math]::Min(600, $out.Length)))
    }
    $script:installed = $true
    "winget install exited " + $p.ExitCode
}

# (3) locate the binary
$exe = $null
Step "locate-resolve" {
    if (-not $script:installed) { throw "not installed" }
    $cands = @(
        "C:\Program Files\Blackmagic Design\DaVinci Resolve\resolve.exe",
        "C:\Program Files\Blackmagic Design\DaVinci Resolve\Resolve.exe"
    )
    foreach ($c in $cands) { if (Test-Path $c) { $script:exe = $c; break } }
    if (-not $script:exe) {
        $hit = Get-ChildItem "C:\Program Files\Blackmagic Design" -Recurse `
            -Filter "resolve.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($hit) { $script:exe = $hit.FullName }
    }
    if (-not $script:exe) { throw "resolve.exe not found under Program Files\Blackmagic Design" }
    $script:exe
}

# (4) launch probe: can Resolve even START on a GPU-less runner?
$launch = [ordered]@{ attempted = $false }
Step "launch-resolve" {
    if (-not $script:exe) { throw "no exe to launch" }
    $launch.attempted = $true
    $t0 = Get-Date
    $p = Start-Process -FilePath $script:exe -PassThru
    $title = ""
    $deadline = (Get-Date).AddSeconds($LaunchTimeoutSec)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 5
        if ($p.HasExited) { break }
        try { $p.Refresh(); $title = $p.MainWindowTitle } catch { }
    }
    if ($p.HasExited) {
        $script:launch["exited_within_$LaunchTimeoutSec" + "s"] = $true
        $script:launch["exit_code"] = $p.ExitCode
        throw ("resolve.exe exited code {0} within {1} s (GPU-less runner?)" -f $p.ExitCode, $LaunchTimeoutSec)
    }
    try { $p.Kill() } catch { }
    $script:launch["survived_sec"] = [int]((Get-Date) - $t0).TotalSeconds
    $script:launch["main_window_title"] = $title
    if (-not $title) { throw "process alive but no window title after " + $LaunchTimeoutSec + " s" }
    "launched; window title: [" + $title + "]"
}
$r.steps["launch"] = $launch

# --- verdict: what CAN CI honestly claim? ------------------------------------
$canCi = $false
if ($r.steps["launch"].ok) { $canCi = $true }
$r["verdict"] = [ordered]@{
    resolve_runs_on_ci_runner = $canCi
    meaning = $(if ($canCi) {
        "Resolve launched on the runner -- H4/H5 automation is worth building next"
    } else {
        "Resolve cannot run on GPU-less GitHub runners -- H4/H5 stay studio-legs " +
        "(the CI-executable NLE coverage is headless Blender through CfAPI: " +
        "scripts/win_nle_matrix.ps1 W3; timeline-level realism is the pinned " +
        "real-NLE corpus gate)"
    })
}

$outDir = Split-Path -Parent $Out
if ($outDir -and -not (Test-Path $outDir)) { New-Item -ItemType Directory -Force -Path $outDir | Out-Null }
$json = ($r | ConvertTo-Json -Depth 8)
[System.IO.File]::WriteAllText($Out, $json, (New-Object System.Text.UTF8Encoding($false)))
Write-Output ""
Write-Output ("written: " + $Out)
Write-Output ("verdict: resolve_runs_on_ci_runner = " + $canCi)
exit 0
