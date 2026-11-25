# Builds release binaries and the Windows installer into dist\.
# Needs: Rust (MSVC toolchain), Inno Setup 6 (ISCC.exe).

param([switch]$SkipBuild)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $root 'Cargo.toml'

$version = (Select-String -LiteralPath $manifest -Pattern '^version = "([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "InxtDrive $version"

if (-not $SkipBuild) {
    # Panic messages embed the source path of every crate, which would put the builder's
    # user name in the binaries. The last matching prefix wins, so the broadest goes first.
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $env:CARGO_ENCODED_RUSTFLAGS = @(
        "--remap-path-prefix=$env:USERPROFILE=~",
        "--remap-path-prefix=$cargoHome=cargo",
        "--remap-path-prefix=$root=inxtdrive"
    ) -join [char]0x1f

    # Windows PowerShell treats progress written to stderr as errors under 'Stop'.
    $ErrorActionPreference = 'Continue'
    cargo build --release -p inxtdrive -p inxt-overlay --manifest-path $manifest
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) { throw 'cargo build failed' }
}

& (Join-Path $root 'installer\fetch-winfsp.ps1')

$candidates = @(
    $env:ISCC,
    (Join-Path $root '.tools\inno\ISCC.exe'),
    (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
    (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe'),
    (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe')
)
$iscc = $candidates | Where-Object { $_ -and (Test-Path -LiteralPath $_) } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 not found. Install it from https://jrsoftware.org/isdl.php or set $env:ISCC.' }

$ErrorActionPreference = 'Continue'
& $iscc "/DAppVersion=$version" (Join-Path $root 'installer\InxtDrive.iss')
$code = $LASTEXITCODE
$ErrorActionPreference = 'Stop'
if ($code -ne 0) { throw 'installer build failed' }
Get-ChildItem -LiteralPath (Join-Path $root 'dist') -Filter "InxtDrive-Setup-$version.exe" | Select-Object FullName, Length
