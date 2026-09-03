# win_nle_stack.ps1 -- Windows NLE-matrix stack bootstrap (Round 13).
#
# Boots the full Cairn stack on a Windows box (CI runner or studio box) for
# the NLE test matrix: storage server (dev insecure, loopback) + daemons
# (device A = author, device B = cold placeholder view), enrolls and logs
# devices in, attaches roots (the daemon registers the CfAPI sync root +
# bulk placeholders on attach -- win_attach.rs).
#
# Pure ASCII (Windows PowerShell 5.1 reads BOM-less ps1 as ANSI cp1252;
# non-ASCII would mangle into parse errors -- the Round 10 lesson).
#
# Usage (see .github/workflows/nle-matrix.yml for the full driving sequence):
#   . scripts/win_nle_stack.ps1 -Cairn C:\path\to\cairn.exe
#   Start-CairnServer -DataDir C:\nle\server
#   Start-CairnDaemon -HomeDir C:\nle\homeA -CtlPort 17777 -Log C:\nle\daemonA.log
#   Invoke-CairnLogin -HomeDir C:\nle\homeA -Server 127.0.0.1:7443 -Name deviceA
#
# NOTE: the home parameter is -HomeDir (NOT -Home): $HOME is a read-only
# PowerShell automatic variable; binding a -Home parameter throws
# "Cannot overwrite variable Home". Caught by the round-13 linux dry run.

param(
    [string]$Cairn = ""
)

# Resolve the cairn binary: explicit param > repo target/release > PATH.
# (forward slashes: valid on windows too, and they keep the non-windows
# machinery dry-run honest)
if (-not $Cairn) {
    $repo = Split-Path -Parent $PSScriptRoot
    $rel = Join-Path $repo "target/release/cairn.exe"
    $dbg = Join-Path $repo "target/debug/cairn.exe"
    if (Test-Path $rel) { $Cairn = $rel }
    elseif (Test-Path $dbg) { $Cairn = $dbg }
    else { $Cairn = "cairn" }
}
$script:CairnExe = (Resolve-Path $Cairn -ErrorAction SilentlyContinue).Path
if (-not $script:CairnExe) { $script:CairnExe = $Cairn }
$script:StackProcesses = @()

function Format-Arg {
    # PS 5.1-safe command-line quoting (Start-Process / ProcessStartInfo
    # Arguments join with spaces WITHOUT quoting on .NET Framework).
    # Our arg set is paths + flags: no embedded quotes; a trailing backslash
    # before a closing quote would escape it, so trim it first.
    param([string]$Value)
    if ($Value -match '[\s"]') {
        $v = $Value.TrimEnd('\')
        return '"' + $v + '"'
    }
    return $Value
}

function Invoke-Cairn {
    # Run a cairn CLI command to completion; returns { Code, Out, Err }.
    param(
        [Parameter(Mandatory=$true)][string[]]$Args,
        [string]$HomeDir = "",
        [int]$TimeoutSec = 120
    )
    $full = New-Object System.Collections.Generic.List[string]
    if ($HomeDir) { $full.Add("--home"); $full.Add($HomeDir) }
    foreach ($a in $Args) { $full.Add($a) }
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $script:CairnExe
    # ArgumentList is .NET Core+ only; PS 5.1 (.NET Framework) needs the
    # quoted Arguments string -- use whichever the runtime offers
    if ($psi.PSObject.Properties["ArgumentList"]) {
        foreach ($a in $full) { [void]$psi.ArgumentList.Add($a) }
    } else {
        $parts = @($full | ForEach-Object { Format-Arg $_ })
        $psi.Arguments = ($parts -join " ")
    }
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    if (-not $p.WaitForExit($TimeoutSec * 1000)) {
        try { $p.Kill() } catch { }
        throw "cairn $($full -join ' ') timed out after $TimeoutSec s"
    }
    $out = $p.StandardOutput.ReadToEnd()
    $err = $p.StandardError.ReadToEnd()
    return [pscustomobject]@{ Code = $p.ExitCode; Out = $out; Err = $err }
}

function Wait-TcpPort {
    param(
        [Parameter(Mandatory=$true)][string]$Address,
        [int]$Port,
        [int]$TimeoutSec = 60
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        $c = New-Object System.Net.Sockets.TcpClient
        try {
            $ok = $c.BeginConnect($Address, $Port, $null, $null)
            if ($ok.AsyncWaitHandle.WaitOne(1000) -and $c.Connected) {
                return $true
            }
        } catch { } finally { try { $c.Close() } catch { } }
        Start-Sleep -Milliseconds 500
    }
    throw "port ${Address}:${Port} never opened (timeout ${TimeoutSec}s)"
}

function Start-BackgroundProcess {
    # Start-Process with stdout+stderr redirect to files (PS 5.1-compatible:
    # -ArgumentList does NOT auto-quote on .NET Framework, so pre-quote via
    # Format-Arg; elements with no special chars pass through unchanged).
    param(
        [Parameter(Mandatory=$true)][string]$FilePath,
        [Parameter(Mandatory=$true)][string[]]$ArgList,
        [Parameter(Mandatory=$true)][string]$Log
    )
    $errLog = "$Log.err"
    $quoted = @($ArgList | ForEach-Object { Format-Arg $_ })
    $p = Start-Process -FilePath $FilePath -ArgumentList $quoted `
        -RedirectStandardOutput $Log -RedirectStandardError $errLog `
        -NoNewWindow -PassThru
    $script:StackProcesses += $p
    return $p
}

function Start-CairnServer {
    param(
        [Parameter(Mandatory=$true)][string]$DataDir,
        [string]$GrpcAddr = "127.0.0.1:7443",
        [string]$ObjectsAddr = "127.0.0.1:7444",
        [string]$Log = ""
    )
    if (-not $Log) { $Log = Join-Path $DataDir "server.log" }
    New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
    $p = Start-BackgroundProcess -FilePath $script:CairnExe -Log $Log -ArgList @(
        "server", "--dev-insecure", "--data-dir", $DataDir,
        "--grpc-addr", $GrpcAddr, "--objects-addr", $ObjectsAddr
    )
    $parts = $GrpcAddr -split ":"
    # [void]: an uncaptured Wait-TcpPort return would flow into this
    # function's pipeline output and callers would get [bool, Process]
    # instead of the Process handle (caught by the W5 restart in the dry run)
    [void](Wait-TcpPort -Address $parts[0] -Port ([int]$parts[1]) -TimeoutSec 120)
    return $p
}

function Start-CairnDaemon {
    param(
        [Parameter(Mandatory=$true)][string]$HomeDir,
        [int]$CtlPort = 17777,
        [int]$UiPort = 17778,
        [Parameter(Mandatory=$true)][string]$Log
    )
    New-Item -ItemType Directory -Force -Path $HomeDir | Out-Null
    # RUST_LOG=info so hydration/sync metric lines land in the log (the I1
    # collector greps them); inherited by Start-Process.
    $env:RUST_LOG = "info"
    $p = Start-BackgroundProcess -FilePath $script:CairnExe -Log $Log -ArgList @(
        "--home", $HomeDir, "daemon",
        "--ctl-addr", "127.0.0.1:$CtlPort",
        "--ui-addr", "127.0.0.1:$UiPort"
    )
    [void](Wait-TcpPort -Address "127.0.0.1" -Port $CtlPort -TimeoutSec 60)
    return $p
}

function Invoke-CairnLogin {
    # dev enroll + login for one device home (server must run --dev-insecure)
    param(
        [Parameter(Mandatory=$true)][string]$HomeDir,
        [Parameter(Mandatory=$true)][string]$Server,
        [string]$Name = "nle-box",
        [string]$Email = "editor@studio.tv",
        [string]$Tenant = "t1"
    )
    $r = Invoke-Cairn -Args @("dev-enroll-code", "--server", $Server,
                              "--tenant", $Tenant, "--email", $Email) -HomeDir $HomeDir
    if ($r.Code -ne 0) { throw "dev-enroll-code failed: $($r.Err) $($r.Out)" }
    # the code prints as an enr-... token (grab it from stdout/stderr)
    $code = $null
    foreach ($line in (($r.Out + "`n" + $r.Err) -split "`n")) {
        if ($line -match "(enr-[A-Za-z0-9_\-]+)") { $code = $Matches[1]; break }
    }
    if (-not $code) { throw "no enroll code found in output: $($r.Out) $($r.Err)" }
    $r2 = Invoke-Cairn -Args @("login", "--server", $Server, "--code", $code,
                               "--name", $Name) -HomeDir $HomeDir
    if ($r2.Code -ne 0) { throw "login failed: $($r2.Err) $($r2.Out)" }
    return $code
}

function Invoke-CairnAttach {
    param(
        [Parameter(Mandatory=$true)][string]$HomeDir,
        [Parameter(Mandatory=$true)][string]$Root,
        [string]$Project = "",
        [int]$CtlPort = 17777
    )
    New-Item -ItemType Directory -Force -Path $Root | Out-Null
    $argList = @("attach", $Root, "--ctl", "http://127.0.0.1:$CtlPort")
    if ($Project) { $argList += @("--project", $Project) }
    $r = Invoke-Cairn -Args $argList -HomeDir $HomeDir -TimeoutSec 300
    if ($r.Code -ne 0) { throw "attach failed: $($r.Err) $($r.Out)" }
    return $r.Out
}

function Get-CairnStatus {
    # NOTE: `cairn status` takes NO --ctl flag (the ctl endpoint resolves
    # from the home store -- each daemon persists ctl/addr at boot, so
    # --home alone routes to the right daemon). Passing --ctl was a clap
    # error (exit 2) that surfaced as a null-Status binding throw in the
    # linux dry run.
    param(
        [string]$HomeDir = "",
        [int]$CtlPort = 17777   # unused by the command; kept for call-site clarity
    )
    return Invoke-Cairn -Args @("status", "--json") -HomeDir $HomeDir -TimeoutSec 60
}

function Get-CairnStatusJson {
    # `status --json` parsed into an object (PS 5.1 ConvertFrom-Json).
    # Returns $null when the daemon is unreachable or the JSON is unparsable.
    param(
        [string]$HomeDir = "",
        [int]$CtlPort = 17777
    )
    $r = Get-CairnStatus -HomeDir $HomeDir -CtlPort $CtlPort
    if ($r.Code -ne 0) { return $null }
    try { return ($r.Out | ConvertFrom-Json) } catch { return $null }
}

function Find-CairnProject {
    # the project entry from a parsed status object (or $null)
    param(
        [Parameter(Mandatory=$true)]$Status,
        [Parameter(Mandatory=$true)][string]$Project
    )
    if (-not $Status) { return $null }
    if (-not $Status.projects) { return $null }
    foreach ($p in @($Status.projects)) {
        if ($p.project_id -eq $Project) { return $p }
    }
    return $null
}

function Wait-ProjectSynced {
    # poll `status --json` until the project reports pending_outbox == 0.
    # (The JSON field is `pending_outbox` -- an earlier revision of this
    # helper grepped `"outbox_pending"`/`"outbox"`/`"pending"` and would
    # have matched nothing, always timing out. Parse, don't grep.)
    param(
        [Parameter(Mandatory=$true)][string]$HomeDir,
        [Parameter(Mandatory=$true)][string]$Project,
        [int]$CtlPort = 17777,
        [int]$TimeoutSec = 300
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    $last = ""
    while ((Get-Date) -lt $deadline) {
        $st = Get-CairnStatusJson -HomeDir $HomeDir -CtlPort $CtlPort
        $p = Find-CairnProject -Status $st -Project $Project
        if ($p) {
            $last = $st | ConvertTo-Json -Depth 5 -Compress
            if ([int]$p.pending_outbox -eq 0) { return $st }
        }
        Start-Sleep -Seconds 2
    }
    throw "project $Project did not converge in ${TimeoutSec}s; last status: $last"
}

function Wait-CairnProjectAttached {
    # poll `status --json` until the daemon lists the project as attached
    # (used after a daemon restart: `resume_all` re-attaches bound roots)
    param(
        [Parameter(Mandatory=$true)][string]$HomeDir,
        [Parameter(Mandatory=$true)][string]$Project,
        [int]$CtlPort = 17777,
        [int]$TimeoutSec = 120
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        $st = Get-CairnStatusJson -HomeDir $HomeDir -CtlPort $CtlPort
        $p = Find-CairnProject -Status $st -Project $Project
        if ($p) { return $st }
        Start-Sleep -Seconds 2
    }
    throw "project $Project not attached after ${TimeoutSec}s (daemon restart)"
}

function Stop-CairnProcess {
    # kill ONE stack process (the conflict row stops exactly one daemon).
    # best-effort, never throws.
    param(
        [Parameter(Mandatory=$true)]$Process
    )
    try {
        if ($Process -and -not $Process.HasExited) {
            $Process.Kill()
            $Process.WaitForExit(5000) | Out-Null
        }
    } catch { }
}

function Stop-CairnStack {
    # kill everything the stack functions started (best-effort, never throws)
    foreach ($p in $script:StackProcesses) {
        try {
            if ($p -and -not $p.HasExited) {
                $p.Kill()
                $p.WaitForExit(5000) | Out-Null
            }
        } catch { }
    }
}
