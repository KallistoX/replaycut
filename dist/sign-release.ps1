# Sign a published release: download its SHA256SUMS, sign it with minisign,
# verify the signature against the public key built into the updater and
# upload SHA256SUMS.minisig as the third release asset.
#
# Run this on the machine that holds the minisign secret key - never in CI.
# The updater refuses a release without a valid SHA256SUMS.minisig.
#
#   dist\sign-release.ps1 v2.3.0
#   dist\sign-release.ps1 v2.3.0 -SecretKey D:\keys\replaycut.key
#
# Needs: minisign (winget install jedisct1.minisign), and either gh (to
# upload) or a browser (the script tells you what to upload where).

param(
    [Parameter(Mandatory = $true)][string]$Tag,
    [string]$Repo = "KallistoX/replaycut",
    [string]$SecretKey = "$env:USERPROFILE\.minisign\minisign.key",
    [string]$WorkDir = "$env:TEMP\replaycut-sign"
)

$ErrorActionPreference = "Stop"
if (-not (Get-Command minisign -ErrorAction SilentlyContinue)) {
    throw "minisign not found - winget install jedisct1.minisign"
}
if (-not (Test-Path $SecretKey)) {
    throw "secret key not found at $SecretKey (pass -SecretKey)"
}

# the public key the updater trusts: from update.rs, so the two cannot drift
$updateRs = Join-Path $PSScriptRoot "..\crates\replaycut\src\update.rs"
$keys = Select-String -Path $updateRs -Pattern '^\s*"([A-Za-z0-9+/=]{56})",?\s*$' |
    ForEach-Object { $_.Matches[0].Groups[1].Value }
if (-not $keys) { throw "no public key found in $updateRs (PUBLIC_KEYS is empty)" }

New-Item -ItemType Directory -Force $WorkDir | Out-Null
Set-Location $WorkDir
Remove-Item SHA256SUMS, SHA256SUMS.minisig -ErrorAction SilentlyContinue

$sums = "https://github.com/$Repo/releases/download/$Tag/SHA256SUMS"
Write-Host "downloading $sums"
Invoke-WebRequest -Uri $sums -OutFile SHA256SUMS -UseBasicParsing
Get-Content SHA256SUMS

# -t puts the tag into the trusted comment; the comment is signed too
& minisign -Sm SHA256SUMS -s $SecretKey -t "replaycut $Tag"
if ($LASTEXITCODE -ne 0) { throw "minisign failed" }

$ok = $false
foreach ($k in $keys) {
    & minisign -Vm SHA256SUMS -P $k -q
    if ($LASTEXITCODE -eq 0) { $ok = $true; Write-Host "verified against $k" }
}
if (-not $ok) { throw "the signature does not verify against the key built into update.rs - wrong secret key?" }

if (Get-Command gh -ErrorAction SilentlyContinue) {
    & gh release upload $Tag SHA256SUMS.minisig --repo $Repo --clobber
    if ($LASTEXITCODE -ne 0) { throw "gh release upload failed" }
    Write-Host "uploaded SHA256SUMS.minisig to $Tag"
} else {
    Write-Host ""
    Write-Host "gh is not installed. Upload this file as an asset of the release ${Tag}:"
    Write-Host "  $WorkDir\SHA256SUMS.minisig"
    Write-Host "  https://github.com/$Repo/releases/edit/$Tag"
}
