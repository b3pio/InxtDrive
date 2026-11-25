# Downloads the WinFsp installer that InxtDrive bundles, and verifies it.
# WinFsp (https://winfsp.dev) is GPLv3 with a FLOSS exception and explicitly
# allows redistribution of its unmodified installer.

param(
    [string]$Version = '2.1',
    [string]$Build = '2.1.25156',
    [string]$Sha256 = '',
    [string]$OutDir = (Join-Path $PSScriptRoot 'redist')
)

$ErrorActionPreference = 'Stop'
$name = "winfsp-$Build.msi"
$url = "https://github.com/winfsp/winfsp/releases/download/v$Version/$name"
$target = Join-Path $OutDir $name

if (-not $Sha256) {
    $pinned = Join-Path $PSScriptRoot 'winfsp.sha256'
    if (Test-Path -LiteralPath $pinned) { $Sha256 = (Get-Content -LiteralPath $pinned -Raw).Trim() }
}

[System.IO.Directory]::CreateDirectory($OutDir) | Out-Null
if (-not (Test-Path -LiteralPath $target)) {
    Write-Host "Downloading $url"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    (New-Object System.Net.WebClient).DownloadFile($url, $target)
}

$sig = Get-AuthenticodeSignature -LiteralPath $target
if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch 'Navimatics') {
    Remove-Item -LiteralPath $target -Force
    throw "WinFsp installer signature is not valid ($($sig.Status), $($sig.SignerCertificate.Subject))"
}
$hash = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash
if ($Sha256 -and $hash -ne $Sha256) {
    Remove-Item -LiteralPath $target -Force
    throw "WinFsp installer hash mismatch: expected $Sha256, got $hash"
}
Write-Host "WinFsp $Build ok ($hash)"
