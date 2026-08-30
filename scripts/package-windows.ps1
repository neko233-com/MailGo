[CmdletBinding()]
param(
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$OutputDirectory = if ($OutputDirectory) { $OutputDirectory } else { Join-Path $projectRoot 'artifacts' }
$targetRoot = Join-Path $env:LOCALAPPDATA 'MailGo\cargo-target'
$package = Get-Content (Join-Path $projectRoot 'package.json') -Raw | ConvertFrom-Json
$version = [string]$package.version
$stageRoot = Join-Path $OutputDirectory "MailGo-$version-windows-x64"
$archivePath = Join-Path $OutputDirectory "MailGo-$version-windows-x64.zip"

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
if (Test-Path -LiteralPath $stageRoot) {
    Remove-Item -LiteralPath $stageRoot -Recurse -Force
}
if (Test-Path -LiteralPath $archivePath) {
    Remove-Item -LiteralPath $archivePath -Force
}

Push-Location $projectRoot
try {
    & npm run build
    if ($LASTEXITCODE -ne 0) { throw "npm run build failed with exit code $LASTEXITCODE" }

    & rdesktop build --path $projectRoot
    if ($LASTEXITCODE -ne 0) { throw "rdesktop build failed with exit code $LASTEXITCODE" }

    $manifest = Join-Path $projectRoot 'native\Cargo.toml'
    & cargo build --release --manifest-path $manifest --locked --target-dir $targetRoot
    if ($LASTEXITCODE -ne 0) { throw "cargo release build failed with exit code $LASTEXITCODE" }
}
finally {
    Pop-Location
}

$nativeExecutable = Join-Path $targetRoot 'release\mailgo-native.exe'
if (-not (Test-Path -LiteralPath $nativeExecutable)) {
    throw "release executable is missing: $nativeExecutable"
}

$distDestination = Join-Path $stageRoot 'dist'
$iconDestination = Join-Path $stageRoot 'resources\icons'
New-Item -ItemType Directory -Force -Path $distDestination | Out-Null
New-Item -ItemType Directory -Force -Path $iconDestination | Out-Null
Copy-Item -Path (Join-Path $projectRoot 'dist\*') -Destination $distDestination -Recurse -Force
Copy-Item -LiteralPath $nativeExecutable -Destination (Join-Path $stageRoot 'MailGo.exe') -Force
Copy-Item -LiteralPath (Join-Path $projectRoot 'resources\icons\mailgo.ico') -Destination (Join-Path $iconDestination 'mailgo.ico') -Force
Compress-Archive -Path (Join-Path $stageRoot '*') -DestinationPath $archivePath -CompressionLevel Optimal

$archive = Get-Item -LiteralPath $archivePath
Write-Host "Portable Windows package created: $($archive.FullName) ($($archive.Length) bytes)"
