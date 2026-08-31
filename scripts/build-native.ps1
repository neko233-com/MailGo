param(
    [switch]$Run,
    [switch]$Release
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$targetRoot = Join-Path $env:LOCALAPPDATA 'MailGo\cargo-target'
$manifest = Join-Path $projectRoot 'native\Cargo.toml'

New-Item -ItemType Directory -Force -Path $targetRoot | Out-Null
$cargoCommand = if ($Run) { 'run' } else { 'build' }
$cargoArguments = @('--manifest-path', $manifest, '--locked', '--target-dir', $targetRoot)
if ($Release) {
    $cargoArguments += '--release'
}

& cargo $cargoCommand @cargoArguments
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
