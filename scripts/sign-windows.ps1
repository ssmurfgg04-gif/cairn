# Windows code signing for release artifacts (review #72).
#
# Signs every path given on the command line with the certificate whose
# thumbprint is in $env:CAIRN_SIGN_THUMBPRINT (imported earlier in the
# release job from the WINDOWS_CERT_PFX_BASE64 / WINDOWS_CERT_PASSWORD
# repository secrets). With no certificate configured it prints a GitHub
# notice and exits 0: unsigned builds keep shipping (SmartScreen warns)
# until a certificate is added, and nothing else in the pipeline changes.
#
# Usage: pwsh scripts/sign-windows.ps1 target\release\cairn.exe ...
param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Paths)
$ErrorActionPreference = "Stop"

if (-not $env:CAIRN_SIGN_THUMBPRINT) {
  Write-Output "::notice::code signing skipped: no WINDOWS_CERT_PFX_BASE64 secret (artifacts ship unsigned)"
  exit 0
}
$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" |
  Sort-Object FullName -Descending | Select-Object -First 1
if (-not $signtool) { throw "signtool.exe not found (Windows SDK missing on the runner)" }

foreach ($p in $Paths) {
  if (-not (Test-Path $p)) { throw "nothing to sign at $p" }
  & $signtool.FullName sign /sha1 $env:CAIRN_SIGN_THUMBPRINT /fd SHA256 `
    /tr http://timestamp.digicert.com /td SHA256 /d "Cairn" $p
  if ($LASTEXITCODE -ne 0) { throw "signtool failed for $p" }
  & $signtool.FullName verify /pa $p
  if ($LASTEXITCODE -ne 0) { throw "signature did not verify for $p" }
  Write-Output "signed + verified: $p"
}
