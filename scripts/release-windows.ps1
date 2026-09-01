[CmdletBinding()]
param(
    [string]$OutputDirectory,
    [switch]$Publish,
    [string]$Tag,
    [string]$NotesFile,
    [switch]$AllowDirty
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot

function Get-Sha256([string]$Path) {
    $algorithm = [System.Security.Cryptography.SHA256]::Create()
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        return ([System.BitConverter]::ToString($algorithm.ComputeHash($stream))).Replace('-', '')
    } finally {
        $stream.Dispose()
        $algorithm.Dispose()
    }
}

$OutputDirectory = if ($OutputDirectory) {
    $resolvedOutput = if ([System.IO.Path]::IsPathRooted($OutputDirectory)) {
        $OutputDirectory
    } else {
        Join-Path $projectRoot $OutputDirectory
    }
    [System.IO.Path]::GetFullPath($resolvedOutput)
} else {
    Join-Path $projectRoot 'artifacts'
}

if ($Publish -and [string]::IsNullOrWhiteSpace($Tag)) {
    throw 'publishing requires an explicit -Tag (for example: -Tag v0.1.0)'
}

Push-Location $projectRoot
try {
    if (-not $AllowDirty) {
        $status = & git status --porcelain --untracked-files=all
        if ($LASTEXITCODE -ne 0) { throw 'could not inspect the Git working tree' }
        if ($status) {
            throw 'working tree is not clean; commit the release or pass -AllowDirty explicitly'
        }
    }

    $package = Get-Content 'package.json' -Raw | ConvertFrom-Json
    $version = [string]$package.version
    $targetRoot = Join-Path $env:LOCALAPPDATA 'MailGo\cargo-target'

    & npm run build
    if ($LASTEXITCODE -ne 0) { throw "npm run build failed with exit code $LASTEXITCODE" }

    & npm run test:custom-css
    if ($LASTEXITCODE -ne 0) { throw "custom CSS checks failed with exit code $LASTEXITCODE" }

    & npm run test:threading
    if ($LASTEXITCODE -ne 0) { throw "conversation threading checks failed with exit code $LASTEXITCODE" }

    & cargo fmt --manifest-path 'native\Cargo.toml' -- --check
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt check failed with exit code $LASTEXITCODE" }

    & cargo clippy --manifest-path 'native\Cargo.toml' --all-targets --all-features --locked --target-dir $targetRoot -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw "cargo clippy failed with exit code $LASTEXITCODE" }

    & cargo test --manifest-path 'native\Cargo.toml' --locked --release --target-dir $targetRoot
    if ($LASTEXITCODE -ne 0) { throw "cargo release tests failed with exit code $LASTEXITCODE" }

    & powershell -NoProfile -ExecutionPolicy Bypass -File 'scripts\package-windows.ps1' -OutputDirectory $OutputDirectory
    if ($LASTEXITCODE -ne 0) { throw "Windows packaging failed with exit code $LASTEXITCODE" }

    $archivePath = Join-Path $OutputDirectory "MailGo-$version-windows-x64.zip"
    if (-not (Test-Path -LiteralPath $archivePath)) {
        throw "release archive is missing: $archivePath"
    }
    $archive = Get-Item -LiteralPath $archivePath
    $commit = (& git rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'could not resolve the release commit' }
    $rdesktopVersion = ((& rdesktop --version 2>$null | Select-Object -First 1) -as [string]).Trim()
    $manifest = [ordered]@{
        product = 'MailGo'
        version = $version
        platform = 'windows-x64'
        commit = $commit
        rdesktop = $rdesktopVersion
        archive = $archive.Name
        bytes = $archive.Length
        sha256 = Get-Sha256 $archive.FullName
        generatedAt = [DateTime]::UtcNow.ToString('o')
        automatedPublishing = $false
    }
    $manifestPath = Join-Path $OutputDirectory "MailGo-$version-windows-x64.manifest.json"
    $manifest | ConvertTo-Json | Set-Content -LiteralPath $manifestPath -Encoding utf8
    Write-Host "Release artifact verified: $($archive.FullName)"
    Write-Host "SHA-256: $($manifest.sha256)"
    Write-Host "Manifest: $manifestPath"

    if ($Publish) {
        $gh = Get-Command gh -ErrorAction Stop
        & $gh.Source auth status
        if ($LASTEXITCODE -ne 0) { throw 'GitHub CLI is not authenticated; release was not published' }
        $releaseArgs = @('release', 'create', $Tag, $archive.FullName, '--title', "MailGo $version")
        if ($NotesFile) {
            if (-not (Test-Path -LiteralPath $NotesFile)) { throw "release notes file is missing: $NotesFile" }
            $releaseArgs += @('--notes-file', (Resolve-Path -LiteralPath $NotesFile).Path)
        } else {
            $releaseArgs += @('--generate-notes')
        }
        & $gh.Source @releaseArgs
        if ($LASTEXITCODE -ne 0) { throw "GitHub release publication failed with exit code $LASTEXITCODE" }
        Write-Host "Published release $Tag manually."
    } else {
        Write-Host 'Publication skipped. Re-run with an explicit -Publish -Tag <tag> to publish.'
    }
} finally {
    Pop-Location
}
