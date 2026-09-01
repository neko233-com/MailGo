[CmdletBinding()]
param(
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$OutputDirectory = if ($OutputDirectory) { $OutputDirectory } else { Join-Path $projectRoot 'artifacts' }
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
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

    $remoteFontReferences = Get-ChildItem -LiteralPath (Join-Path $projectRoot 'dist') -File -Recurse |
        Select-String -Pattern 'fonts\.(googleapis|gstatic)\.com' -ErrorAction SilentlyContinue
    if ($remoteFontReferences) {
        throw 'renderer contains a remote font dependency; Windows packages must remain usable offline'
    }

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

$stagedExecutable = Join-Path $stageRoot 'MailGo.exe'
$executableHeader = [System.IO.File]::ReadAllBytes($stagedExecutable)
if ($executableHeader.Length -lt 2 -or $executableHeader[0] -ne 0x4d -or $executableHeader[1] -ne 0x5a) {
    throw 'staged MailGo.exe is not a valid Windows PE executable'
}
$peOffset = [System.BitConverter]::ToInt32($executableHeader, 0x3c)
$subsystemOffset = $peOffset + 24 + 68
if ($peOffset -lt 0 -or $subsystemOffset + 2 -gt $executableHeader.Length) {
    throw 'staged MailGo.exe has an invalid PE optional header'
}
$subsystem = [System.BitConverter]::ToUInt16($executableHeader, $subsystemOffset)
if ($subsystem -ne 2) {
    throw "staged MailGo.exe uses PE subsystem $subsystem; Release builds must use Windows GUI subsystem 2 without a console window"
}
$versionInfo = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($stagedExecutable)
if ($versionInfo.ProductName -ne 'MailGo') {
    throw 'staged MailGo.exe is missing the embedded MailGo Windows resource metadata and application icon'
}
if (-not (Test-Path -LiteralPath (Join-Path $distDestination 'index.html'))) {
    throw 'staged renderer is missing dist\index.html'
}
if (-not (Test-Path -LiteralPath (Join-Path $iconDestination 'mailgo.ico'))) {
    throw 'staged tray icon is missing resources\icons\mailgo.ico'
}
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archiveStream = [System.IO.File]::Create($archivePath)
$zip = [System.IO.Compression.ZipArchive]::new($archiveStream, [System.IO.Compression.ZipArchiveMode]::Create, $false)
try {
    $epoch = [DateTimeOffset]::Parse('1980-01-01T00:00:00Z')
    $files = Get-ChildItem -LiteralPath $stageRoot -File -Recurse | Sort-Object FullName
    foreach ($file in $files) {
        $relativeName = $file.FullName.Substring($stageRoot.Length).TrimStart('\', '/') -replace '\\', '/'
        $entry = $zip.CreateEntry($relativeName, [System.IO.Compression.CompressionLevel]::Optimal)
        $entry.LastWriteTime = $epoch
        $input = [System.IO.File]::OpenRead($file.FullName)
        $output = $entry.Open()
        try {
            $input.CopyTo($output)
        } finally {
            $output.Dispose()
            $input.Dispose()
        }
    }
} finally {
    $zip.Dispose()
    $archiveStream.Dispose()
}

$archive = Get-Item -LiteralPath $archivePath
Write-Host "Portable Windows package created: $($archive.FullName) ($($archive.Length) bytes)"
