# win_nle_matrix.ps1 -- the CI-executable NLE matrix (Round 13, part 2).
#
# The H1-H10 rows of docs/design/nle-test-matrix.md need a studio Windows box
# with a human artist. This script executes the SUBSET that a GitHub Actions
# windows runner can prove HONESTLY -- a real two-device Cairn stack through
# the real CfAPI filter, with a real NLE (headless Blender, bpy wheel) doing
# open -> scrub -> save -> reopen through a cold sync root:
#
#   W0 boot          server + daemon A (author) + daemon B (cold) + enroll +
#                    attach both roots (CfAPI registration + placeholders)
#   W1 seed          BMW27.blend + conflict_probe.txt authored through A's
#                    root (CfAPI write-back ingest -> upload); B reconciles,
#                    probe fully hydrated for W5
#   W2 cold I1       B's CAS is empty: first 2 MiB read of the placeholder =
#                    cold fetch through the full stack (callback -> plane ->
#                    verify -> serve); then full-file SHA256 byte identity
#   W3 real NLE      headless Blender (bpy): open -> scrub -> save -> reopen
#                    through B's CfAPI root (H6+H7 for Windows)
#   W4 propagation   B's Blender save uploads; A's view converges
#                    byte-identical (H4 "re-import on device B" shape)
#   W5 conflict      deterministic H9: B's daemon stopped -> B edits offline,
#                    A edits live (head moves) -> B restarts -> engine
#                    rejects B's stale append -> ONE conflict copy, BOTH
#                    versions recoverable
#   W6 tl contract   the Windows binary speaks the same tl-merge exit-code
#                    contract (0 clean / 3 refused)
#
# Pure ASCII (Windows PowerShell 5.1 reads BOM-less ps1 as ANSI cp1252; the
# Round 10 lesson). PS 5.1-compatible constructs only.
#
# Usage (full driving sequence in .github/workflows/nle-matrix.yml):
#   powershell -File scripts\win_nle_matrix.ps1 -Cairn target\release\cairn.exe
#
# Exit codes: 0 all rows green | 1 any row failed (details in the JSON + stdout)

param(
    [string]$Cairn = "",
    [string]$Python = "python",
    [string]$RootBase = "",
    [string]$Blend = "",
    [string]$Out = "",
    [string]$Project = "p1"
)

$ErrorActionPreference = "Stop"

# --- resolve paths -----------------------------------------------------------
$repo = Split-Path -Parent $PSScriptRoot
if (-not $Cairn) {
    $rel = Join-Path $repo "target/release/cairn.exe"
    $dbg = Join-Path $repo "target/debug/cairn.exe"
    if (Test-Path $rel) { $Cairn = $rel } elseif (Test-Path $dbg) { $Cairn = $dbg } else { $Cairn = "cairn" }
}
if (-not $Blend) { $Blend = Join-Path $repo "crates/cairn-core/tests/data/BMW27.blend" }
if (-not $RootBase) {
    # user-owned dir (no admin for sync-root registration); stable across the
    # daemon restart in W5 (RUNNER_TEMP would work on CI but dies with the job)
    $profile = $env:USERPROFILE
    if (-not $profile) { $profile = $env:HOME }   # non-windows dry runs
    $RootBase = Join-Path $profile "cairn-nle"
}
if (-not $Out) {
    $Out = Join-Path $repo "docs/nle-matrix-results/windows-runner-matrix.json"
}

foreach ($p in @($Cairn, $Blend)) {
    if (-not (Test-Path $p)) { Write-Output "FAIL required path missing: $p"; exit 1 }
}
$Cairn = (Resolve-Path $Cairn).Path
$Blend = (Resolve-Path $Blend).Path

. (Join-Path $PSScriptRoot "win_nle_stack.ps1") -Cairn $Cairn

$N = Join-Path $RootBase "nle"          # scratch: homes + logs + server data
$RootA = Join-Path $RootBase "rootA"    # device A's attached project root
$RootB = Join-Path $RootBase "rootB"    # device B's attached project root
$HomeA = Join-Path $N "homeA"
$HomeB = Join-Path $N "homeB"
$ProjDirA = Join-Path $RootA "project"
$ProjDirB = Join-Path $RootB "project"
$CtlA = 17777; $UiA = 17778
$CtlB = 17779; $UiB = 17780

if (Test-Path $RootBase) {
    # a previous run's sync roots would collide with fresh registration
    Remove-Item -Recurse -Force $RootBase -ErrorAction SilentlyContinue
}
foreach ($d in @($N, $HomeA, $HomeB, $ProjDirA, $ProjDirB)) {
    New-Item -ItemType Directory -Force -Path $d | Out-Null
}

# --- results plumbing --------------------------------------------------------
$script:Rows = @()
function New-Row {
    param([Parameter(Mandatory=$true)][string]$Id, [string]$Desc = "")
    return (New-Object pscustomobject -Property ([ordered]@{
        row = $Id; desc = $Desc; ok = $false; ms = $null; detail = ""
    }))
}

function Complete-Row {
    # record the row outcome; print the one-line verdict CI greps
    param(
        [Parameter(Mandatory=$true)]$Row,
        [Parameter(Mandatory=$true)][bool]$Ok,
        [int]$Ms = -1,
        [string]$Detail = ""
    )
    $Row.ok = $Ok
    if ($Ms -ge 0) { $Row.ms = $Ms }
    $Row.detail = $Detail
    $script:Rows += $Row
    $verdict = "PASS"; if (-not $Ok) { $verdict = "FAIL" }
    $msTxt = ""; if ($Row.ms -ne $null) { $msTxt = " ($($Row.ms) ms)" }
    Write-Output ("ROW {0}: {1}{2} {3}" -f $Row.row, $verdict, $msTxt, $Row.desc)
    if ($Detail) { Write-Output ("     {0}" -f ($Detail -replace "`r?`n", " | ")) }
}

function Sha256 {
    param([Parameter(Mandatory=$true)][string]$Path)
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash
}

function Read-FirstBytes {
    # open + read up to $Len bytes through the CfAPI filter; returns wall ms
    # (double). Throws on I/O failure (a failed hydration must be loud).
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][int]$Len
    )
    $buf = New-Object byte[] $Len
    $fs = $null
    try {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $fs = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
        $got = 0
        while ($got -lt $Len) {
            $n = $fs.Read($buf, $got, $Len - $got)
            if ($n -le 0) { break }
            $got += $n
        }
        $sw.Stop()
        return @{ ms = $sw.Elapsed.TotalMilliseconds; got = $got }
    } finally {
        if ($fs) { $fs.Dispose() }
    }
}

function Dump-Diagnostics {
    # NEVER throws: a dead run must still show the daemon/server logs
    Write-Output "=== diagnostics begin ==="
    foreach ($log in @(
        (Join-Path $N "daemonA.log"), (Join-Path $N "daemonB.log"),
        (Join-Path $N "daemonB2.log"), (Join-Path $N "server.log"))) {
        if (Test-Path $log) {
            Write-Output "--- $log (tail 40) ---"
            Get-Content $log -Tail 40 -ErrorAction SilentlyContinue | ForEach-Object { Write-Output "  $_" }
        }
    }
    foreach ($root in @($RootA, $RootB)) {
        Write-Output "--- $root ---"
        Get-ChildItem -Recurse $root -ErrorAction SilentlyContinue |
            Select-Object -First 30 | ForEach-Object { Write-Output ("  {0} {1}" -f $_.FullName, $_.Length) }
    }
    Write-Output "=== diagnostics end ==="
}

# --- the matrix ----------------------------------------------------------------
$failFast = $false   # run every row; the JSON records the full picture
$green = $true
$metrics = [ordered]@{}
$blenderStages = @()
try {
    # ---- W0: boot the two-device stack -----------------------------------
    $rowW0 = New-Row "W0" "boot: server + daemonA + daemonB + enroll + attach x2"
    $row = $rowW0
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $server = Start-CairnServer -DataDir (Join-Path $N "server")
        $daemonA = Start-CairnDaemon -HomeDir $HomeA -CtlPort $CtlA -UiPort $UiA `
            -Log (Join-Path $N "daemonA.log")
        $daemonB = Start-CairnDaemon -HomeDir $HomeB -CtlPort $CtlB -UiPort $UiB `
            -Log (Join-Path $N "daemonB.log")
        [void](Invoke-CairnLogin -HomeDir $HomeA -Server "127.0.0.1:7443" -Name "deviceA")
        [void](Invoke-CairnLogin -HomeDir $HomeB -Server "127.0.0.1:7443" -Name "deviceB")
        # attach creates project p1 (ensure_project) on A first; B joins it
        [void](Invoke-CairnAttach -HomeDir $HomeA -Root $RootA -Project $Project -CtlPort $CtlA)
        [void](Invoke-CairnAttach -HomeDir $HomeB -Root $RootB -Project $Project -CtlPort $CtlB)
        $sw.Stop()
        Complete-Row $row $true ([int]$sw.ElapsedMilliseconds) "stack up; both roots attached"
    } catch {
        $sw.Stop()
        $green = $false
        Complete-Row $row $false ([int]$sw.ElapsedMilliseconds) ("boot failed: " + $_.Exception.Message)
        Dump-Diagnostics
        if ($failFast) { throw }
    }

    $rowW1 = New-Row "W1" "seed: BMW27.blend + probe through A's root -> upload; B reconciles + hydrates probe"
    $row = $rowW1
    if ($rowW0.ok) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            Copy-Item -LiteralPath $Blend -Destination (Join-Path $ProjDirA "scene.blend")
            $probeBytes = New-Object byte[] 131072
            for ($i = 0; $i -lt $probeBytes.Length; $i++) { $probeBytes[$i] = $i % 251 }
            [System.IO.File]::WriteAllBytes((Join-Path $ProjDirA "conflict_probe.txt"), $probeBytes)
            $seedSceneSha = Sha256 -Path (Join-Path $ProjDirA "scene.blend")
            $seedProbeSha = Sha256 -Path (Join-Path $ProjDirA "conflict_probe.txt")
            # A ingests + uploads both
            [void](Wait-ProjectSynced -HomeDir $HomeA -Project $Project -CtlPort $CtlA -TimeoutSec 600)
            # B's engine reconciles the new heads into its root (placeholders);
            # full read of the probe NOW so W5's offline edit has a materialized file
            $deadline = (Get-Date).AddSeconds(300)
            $probeB = $null
            while ((Get-Date) -lt $deadline) {
                $probeB = Join-Path $ProjDirB "conflict_probe.txt"
                if ((Test-Path $probeB) -and ((Test-Path (Join-Path $ProjDirB "scene.blend")))) { break }
                Start-Sleep -Seconds 2
            }
            if (-not (Test-Path $probeB)) { throw "B never saw the seeded files (reconcile timeout)" }
            $probeRead = Read-FirstBytes -Path $probeB -Len 131072
            if ($probeRead.got -ne 131072) { throw ("probe hydration short read: " + $probeRead.got) }
            [void](Wait-ProjectSynced -HomeDir $HomeB -Project $Project -CtlPort $CtlB -TimeoutSec 300)
            $sw.Stop()
            Complete-Row $row $true ([int]$sw.ElapsedMilliseconds) `
                "scene.blend + conflict_probe.txt seeded via A; B reconciled (files_synced reached)"
        } catch {
            $sw.Stop(); $green = $false
            Complete-Row $row $false ([int]$sw.ElapsedMilliseconds) ("seed failed: " + $_.Exception.Message)
            Dump-Diagnostics
        }
    }

    $rowW2 = New-Row "W2" "cold I1: first 2 MiB of scene.blend placeholder on B (empty CAS -> full-stack fetch) + full SHA256"
    $row = $rowW2
    if ($rowW1.ok) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            $sceneB = Join-Path $ProjDirB "scene.blend"
            $cold = Read-FirstBytes -Path $sceneB -Len 2097152
            if ($cold.got -ne 2097152) { throw ("cold read short: " + $cold.got + " of 2097152") }
            $warm = @()
            for ($k = 0; $k -lt 2; $k++) { $warm += (Read-FirstBytes -Path $sceneB -Len 2097152).ms }
            $sceneShaB = Sha256 -Path $sceneB
            $identical = ($sceneShaB -eq $seedSceneSha)
            if (-not $identical) { throw "byte identity FAILED: B's scene.blend differs from the seed" }
            $metrics["cold_first_2mib_ms"] = [math]::Round($cold.ms, 2)
            $metrics["warm_first_2mib_ms"] = [math]::Round((($warm | Measure-Object -Minimum).Minimum), 2)
            $metrics["scene_blake_sha256_match"] = $true
            $sw.Stop()
            Complete-Row $row $true ([int]$sw.ElapsedMilliseconds) `
                ("cold {0:N2} ms / warm {1:N2} ms for the first 2 MiB; SHA256 identical" -f $cold.ms, $metrics["warm_first_2mib_ms"])
        } catch {
            $sw.Stop(); $green = $false
            Complete-Row $row $false ([int]$sw.ElapsedMilliseconds) ("cold I1/identity failed: " + $_.Exception.Message)
            Dump-Diagnostics
        }
    }

    $rowW3 = New-Row "W3" "real NLE: headless Blender open -> scrub -> save -> reopen through B's root"
    $row = $rowW3
    if ($rowW2.ok) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            # bpy import check via the Process API (NOT `& $Python ... 2>&1`:
            # with EAP=Stop, PS 5.1 turns native stderr lines into thrown
            # ErrorRecords -- the classic trap)
            $bpyPsi = New-Object System.Diagnostics.ProcessStartInfo
            $bpyPsi.FileName = $Python
            $bpyCmd = "import bpy; print(bpy.app.version_string)"
            if ($bpyPsi.PSObject.Properties["ArgumentList"]) {
                [void]$bpyPsi.ArgumentList.Add("-c"); [void]$bpyPsi.ArgumentList.Add($bpyCmd)
            } else {
                $bpyPsi.Arguments = "-c " + (Format-Arg $bpyCmd)
            }
            $bpyPsi.RedirectStandardOutput = $true
            $bpyPsi.RedirectStandardError = $true
            $bpyPsi.UseShellExecute = $false
            $bpyPsi.CreateNoWindow = $true
            $bpyP = [System.Diagnostics.Process]::Start($bpyPsi)
            $bpyOutT = $bpyP.StandardOutput.ReadToEndAsync()
            $bpyErrT = $bpyP.StandardError.ReadToEndAsync()
            [void]$bpyP.WaitForExit(120000)
            $bpyOut = $bpyOutT.Result.Trim()
            $bpyErr = $bpyErrT.Result.Trim()
            if ($bpyP.ExitCode -ne 0) { throw ("bpy not importable: " + $bpyErr) }

            $sceneB = Join-Path $ProjDirB "scene.blend"
            $psi = New-Object System.Diagnostics.ProcessStartInfo
            $psi.FileName = $Python
            $tscript = Join-Path $repo "scripts/test_cairn.py"
            $targs = @($tscript, "--blend", $sceneB, "--frames", "1-120", "--rounds", "2")
            if ($psi.PSObject.Properties["ArgumentList"]) {
                foreach ($a in $targs) { [void]$psi.ArgumentList.Add($a) }
            } else {
                $psi.Arguments = (@($targs | ForEach-Object { Format-Arg $_ }) -join " ")
            }
            $psi.RedirectStandardOutput = $true
            $psi.RedirectStandardError = $true
            $psi.UseShellExecute = $false
            $psi.CreateNoWindow = $true
            $p = [System.Diagnostics.Process]::Start($psi)
            # async reads: WaitForExit with a full stdout pipe deadlocks
            # (Blender chatters); ReadToEndAsync drains as it fills
            $outT = $p.StandardOutput.ReadToEndAsync()
            $errT = $p.StandardError.ReadToEndAsync()
            if (-not $p.WaitForExit(900000)) { $p.Kill(); throw "test_cairn.py timed out (15 min)" }
            $stdout = $outT.Result
            $stderr = $errT.Result
            Write-Output ("     bpy {0}; test_cairn.py exit {1}" -f $bpyOut, $p.ExitCode)
            foreach ($line in ($stdout -split "`r?`n")) { if ($line -match "^STAGE ") { $blenderStages += $line } }
            if ($blenderStages.Count -gt 0) { Write-Output ("     " + ($blenderStages -join " | ")) }
            if ($p.ExitCode -ne 0) {
                $tail = ($stdout + $stderr) -split "`r?`n" | Select-Object -Last 12
                throw ("test_cairn.py exit " + $p.ExitCode + " (2 = round-trip mismatch): " + ($tail -join " | "))
            }
            $sw.Stop()
            $metrics["blender_exit"] = $p.ExitCode
            Complete-Row $row $true ([int]$sw.ElapsedMilliseconds) "bpy open/scrub/save/reopen green through CfAPI"
        } catch {
            $sw.Stop(); $green = $false
            Complete-Row $row $false ([int]$sw.ElapsedMilliseconds) ("NLE row failed: " + $_.Exception.Message)
            Dump-Diagnostics
        }
    }

    $rowW4 = New-Row "W4" "propagation: B's Blender save uploads; A's view converges byte-identical"
    $row = $rowW4
    if ($rowW3.ok) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            [void](Wait-ProjectSynced -HomeDir $HomeB -Project $Project -CtlPort $CtlB -TimeoutSec 600)
            $sceneB = Join-Path $ProjDirB "scene.blend"
            $postSaveShaB = Sha256 -Path $sceneB
            $sceneA = Join-Path $ProjDirA "scene.blend"
            $converged = $false
            $deadline = (Get-Date).AddSeconds(300)
            while ((Get-Date) -lt $deadline) {
                $shaA = Sha256 -Path $sceneA
                if ($shaA -eq $postSaveShaB) { $converged = $true; break }
                Start-Sleep -Seconds 5
            }
            if (-not $converged) {
                throw ("A's view never matched B's post-save SHA within 300s (last A: " +
                    (Sha256 -Path $sceneA) + " vs B: " + $postSaveShaB + ")")
            }
            $sw.Stop()
            $metrics["cross_device_converge_ms"] = [int]$sw.ElapsedMilliseconds
            Complete-Row $row $true ([int]$sw.ElapsedMilliseconds) "A re-read B's re-saved scene.blend byte-identically"
        } catch {
            $sw.Stop(); $green = $false
            Complete-Row $row $false ([int]$sw.ElapsedMilliseconds) ("propagation failed: " + $_.Exception.Message)
            Dump-Diagnostics
        }
    }

    # W5 is deliberately decoupled from W3/W4: the conflict surface needs
    # only the seeded+hydrated probe (a failed Blender leg must not mask the
    # conflict contract)
    $rowW5 = New-Row "W5" "conflict: offline edit on B vs live edit on A -> ONE conflict copy, both recoverable"
    $row = $rowW5
    if ($rowW1.ok) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            # B's local probe is fully materialized (W1 hydrated all 128 KiB)
            $probeA = Join-Path $ProjDirA "conflict_probe.txt"
            $probeB = Join-Path $ProjDirB "conflict_probe.txt"

            # (1) stop B's daemon: B is now offline with a materialized file
            Stop-CairnProcess -Process $daemonB
            Start-Sleep -Seconds 2
            if (-not $daemonB.HasExited) { throw "daemon B did not stop" }

            # (2) B edits OFFLINE (plain write; the file is fully local and the
            # design's offline contract says hydrated writes succeed, sync on
            # reconnect -- write-back.md section 7)
            $bEdit = [System.Text.Encoding]::ASCII.GetBytes("B-OFFLINE-EDIT-v1")
            $fs = [System.IO.File]::Open($probeB, [System.IO.FileMode]::Append,
                [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
            try { $fs.Write($bEdit, 0, $bEdit.Length) } finally { $fs.Dispose() }

            # (3) A edits LIVE (CfAPI write-back -> upload; the server head moves)
            # "A-LIVE-EDIT-v2" is 14 bytes (byte-count bugs fail LOUDLY here)
            $aEdit = [System.Text.Encoding]::ASCII.GetBytes("A-LIVE-EDIT-v2")
            $fs = [System.IO.File]::Open($probeA, [System.IO.FileMode]::Append,
                [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
            try { $fs.Write($aEdit, 0, $aEdit.Length) } finally { $fs.Dispose() }
            [void](Wait-ProjectSynced -HomeDir $HomeA -Project $Project -CtlPort $CtlA -TimeoutSec 300)
            $shaA = Sha256 -Path $probeA   # A's authoritative current version

            # (4) restart B: resume_all re-attaches the root; the scan sees B's
            #     mtime change -> append with a STALE parent -> server CONFLICT.
            #     WHICH device ends up holding the conflict copy depends on
            #     whose append lands second (edit-discovery timing) -- the
            #     CONTRACT is side-agnostic: exactly ONE copy across BOTH
            #     roots, and BOTH versions recoverable (the original path
            #     converges to the winner everywhere; the copy carries the
            #     loser's bytes). The linux dry run proved this the hard way:
            #     A's scan was slower, so A took the conflict.
            $daemonB2 = Start-CairnDaemon -HomeDir $HomeB -CtlPort $CtlB -UiPort $UiB `
                -Log (Join-Path $N "daemonB2.log")
            [void](Wait-CairnProjectAttached -HomeDir $HomeB -Project $Project -CtlPort $CtlB -TimeoutSec 180)

            $conflictPath = $null
            $converged = $false
            $deadline = (Get-Date).AddSeconds(300)
            while ((Get-Date) -lt $deadline) {
                foreach ($root in @($ProjDirA, $ProjDirB)) {
                    if ($conflictPath) { break }
                    $hits = @(Get-ChildItem -LiteralPath $root -Filter "*.txt" -ErrorAction SilentlyContinue |
                        Where-Object { $_.Name -like "conflict_probe (conflict*" })
                    if ($hits.Count -gt 0) { $conflictPath = $hits[0].FullName }
                }
                $shaAO = $null; $shaBO = $null
                if (Test-Path $probeA) { $shaAO = Sha256 -Path $probeA }
                if (Test-Path $probeB) { $shaBO = Sha256 -Path $probeB }
                if ($conflictPath -and $shaAO -and $shaBO -and ($shaAO -eq $shaBO)) {
                    $converged = $true; break
                }
                Start-Sleep -Seconds 3
            }
            if (-not $conflictPath) { throw "no conflict copy appeared on either root within 300s" }
            if (-not $converged) { throw "original paths on A and B never converged to the same (winner's) version" }
            # exactly ONE DISTINCT copy name; the SAME copy may legitimately
            # sync to the other device ("both versions recoverable" everywhere)
            $nameToPaths = @{}
            foreach ($root in @($ProjDirA, $ProjDirB)) {
                foreach ($hit in @(Get-ChildItem -LiteralPath $root -Filter "*.txt" -ErrorAction SilentlyContinue |
                        Where-Object { $_.Name -like "conflict_probe (conflict*" })) {
                    if (-not $nameToPaths.ContainsKey($hit.Name)) { $nameToPaths[$hit.Name] = @() }
                    $nameToPaths[$hit.Name] += $hit.FullName
                }
            }
            if ($nameToPaths.Count -ne 1) {
                throw ("expected exactly ONE distinct conflict copy, saw: " + ($nameToPaths.Keys -join ", "))
            }
            $copyName = @($nameToPaths.Keys)[0]
            $copyPaths = $nameToPaths[$copyName]
            $conflictPath = $copyPaths[0]

            # BOTH versions recoverable: every physical copy carries the loser's
            # edit (server-consistent across devices); the original path carries
            # the winner's -- in EITHER assignment
            foreach ($cp in $copyPaths) {
                $cb = [System.IO.File]::ReadAllBytes($cp)
                $ct = [System.Text.Encoding]::ASCII.GetString($cb, $cb.Length - 17, 17)
                $copyIsB = $ct.EndsWith("B-OFFLINE-EDIT-v1")
                $copyIsA = $ct.EndsWith("A-LIVE-EDIT-v2")
                if (-not ($copyIsB -or $copyIsA)) {
                    throw ("copy does not carry either edit (tail '" + $ct + "' at " + $cp + ")")
                }
            }
            $origBytes = [System.IO.File]::ReadAllBytes($probeA)
            $origTail = [System.Text.Encoding]::ASCII.GetString($origBytes, $origBytes.Length - 17, 17)
            $origIsB = $origTail.EndsWith("B-OFFLINE-EDIT-v1")
            $origIsA = $origTail.EndsWith("A-LIVE-EDIT-v2")
            $probe0 = [System.IO.File]::ReadAllBytes($copyPaths[0])
            $copyTail = [System.Text.Encoding]::ASCII.GetString($probe0, $probe0.Length - 17, 17)
            $copyIsB = $copyTail.EndsWith("B-OFFLINE-EDIT-v1")
            $copyIsA = $copyTail.EndsWith("A-LIVE-EDIT-v2")
            if (-not (($copyIsB -and $origIsA) -or ($copyIsA -and $origIsB))) {
                throw ("both versions not recoverable: copy tail '" + $copyTail +
                    "' / original tail '" + $origTail + "'")
            }
            $metrics["conflict_copy_name"] = $copyName
            $metrics["conflict_copy_root"] = $(if ($conflictPath.StartsWith($ProjDirA)) { "A" } else { "B" })
            $metrics["conflict_copy_devices"] = $copyPaths.Count
            $sw.Stop()
            Complete-Row $row $true ([int]$sw.ElapsedMilliseconds) `
                ("one conflict copy name [" + $copyName + "], present on " + $copyPaths.Count +
                 " device root(s); original = winner everywhere; both versions recoverable")
        } catch {
            $sw.Stop(); $green = $false
            Complete-Row $row $false ([int]$sw.ElapsedMilliseconds) ("conflict row failed: " + $_.Exception.Message)
            Dump-Diagnostics
        }
    }

    # W6 runs unconditionally: the tl contract only needs the binary + fixtures
    $rowW6 = New-Row "W6" "tl contract: tl-capture + tl-merge exit codes 0 (clean) / 3 (refused) on the Windows binary"
    $row = $rowW6
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $fix = Join-Path $repo "crates/cairn-tl/fixtures/roundtrip_base.otio"
        $tmp = Join-Path $N "tl"
        New-Item -ItemType Directory -Force -Path $tmp | Out-Null
        $base = Join-Path $tmp "base.otio"
        Copy-Item -LiteralPath $fix -Destination $base
        $r1 = Invoke-Cairn -Args @("tl-capture", $base) -HomeDir ""
        if ($r1.Code -ne 0) { throw ("tl-capture failed: " + $r1.Err) }
        $canon = Join-Path $tmp "base.canonical.otio"
        if (-not (Test-Path $canon)) { throw "canonical output missing after tl-capture" }
        $r2 = Invoke-Cairn -Args @("tl-merge", "--base", $canon, "--ours", $canon, "--theirs", $canon) -HomeDir ""
        if ($r2.Code -ne 0) { throw ("clean merge exit " + $r2.Code + " (expected 0): " + $r2.Err) }
        $garbage = Join-Path $tmp "garbage.otio"
        [System.IO.File]::WriteAllText($garbage, '{"OTIO_SCHEMA":"Nope.9"}')
        $r3 = Invoke-Cairn -Args @("tl-merge", "--base", $canon, "--ours", $garbage, "--theirs", $canon) -HomeDir ""
        if ($r3.Code -ne 3) { throw ("refused merge exit " + $r3.Code + " (expected 3)") }
        $sw.Stop()
        Complete-Row $row $true ([int]$sw.ElapsedMilliseconds) "exit codes 0 and 3 verified on windows"
    } catch {
        $sw.Stop(); $green = $false
        Complete-Row $row $false ([int]$sw.ElapsedMilliseconds) ("tl contract failed: " + $_.Exception.Message)
    }
} finally {
    # best-effort clean detach (drops the CfAPI connections) while any daemon
    # still answers its ctl port; then kill the stack. Never throws.
    foreach ($ctlPort in @($CtlA, $CtlB)) {
        try {
            [void](Invoke-Cairn -Args @("detach", "--project", $Project,
                "--ctl", ("http://127.0.0.1:" + $ctlPort)) -HomeDir "" -TimeoutSec 30)
        } catch { }
    }
    Stop-CairnStack
}

# --- the report ------------------------------------------------------------------
$os = [System.Environment]::OSVersion.Version.ToString()
$psv = $PSVersionTable.PSVersion.ToString()
$cpu = $env:PROCESSOR_IDENTIFIER     # windows; absent elsewhere -> unknown
if (-not $cpu) {
    try {
        # Get-CimInstance is windows-only in PS7 and MISSING under pwsh on
        # linux (command-not-found is terminating under EAP=Stop -- it
        # crashed the report stage in the linux dry run)
        $cpu = (Get-CimInstance Win32_Processor -ErrorAction SilentlyContinue |
            Select-Object -First 1).Name
    } catch { $cpu = $null }
}
if (-not $cpu) { $cpu = "unknown" }

$osName = "Windows " + $os
$context = "GitHub Actions windows runner (best-case local I1: loopback plane + NVMe; WAN RTT is the studio leg)"
if ($PSVersionTable.Platform -eq "Unix") {
    # the machinery dry run: on non-windows the CfAPI layer is a no-op, so
    # rows W2/W3 measure plain files, not the filter -- say so honestly
    $osName = "Unix " + $os
    $context = "non-windows MACHINERY DRY RUN (CfAPI layer is a no-op here; filter-specific rows are not meaningful)"
}

$report = [ordered]@{
    schema = "cairn-nle-matrix/windows-runner/1"
    captured_utc = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    green = $green
    host = [ordered]@{
        os = $osName
        powershell = $psv
        cpu = $cpu
        context = $context
    }
    stack = [ordered]@{
        cairn = ((Invoke-Cairn -Args @("--version") -HomeDir "").Out -split "`r?`n" | Select-Object -Last 1)
        project = $Project
        rootA = $RootA
        rootB = $RootB
    }
    metrics = $metrics
    blender_stages = $blenderStages
    rows = $script:Rows
}

$outDir = Split-Path -Parent $Out
if ($outDir -and -not (Test-Path $outDir)) { New-Item -ItemType Directory -Force -Path $outDir | Out-Null }
# BOM-less UTF-8: the conflict copy name carries a U+2014 em-dash (the
# engine's spec naming); ASCII would mangle it to '?'. Set-Content -Encoding
# UTF8 on PS 5.1 writes a BOM, which trips strict JSON parsers.
$json = ($report | ConvertTo-Json -Depth 8)
[System.IO.File]::WriteAllText($Out, $json, (New-Object System.Text.UTF8Encoding($false)))
Write-Output ""
Write-Output ("written: " + $Out)
Write-Output ("verdict: " + $(if ($green) { "MATRIX GREEN" } else { "MATRIX RED" }))
if (-not $green) { exit 1 }
exit 0
