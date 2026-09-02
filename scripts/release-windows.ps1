[CmdletBinding()]
param(
    [string]$OutputDirectory,
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

    & npm run test:async-pool
    if ($LASTEXITCODE -ne 0) { throw "bounded async pool checks failed with exit code $LASTEXITCODE" }

    & npm run test:windows-icons
    if ($LASTEXITCODE -ne 0) { throw "Windows icon checks failed with exit code $LASTEXITCODE" }

    & npm run test:ipc-capability
    if ($LASTEXITCODE -ne 0) { throw "packaged IPC capability checks failed with exit code $LASTEXITCODE" }

    & npm run test:security-policy
    if ($LASTEXITCODE -ne 0) { throw "security policy checks failed with exit code $LASTEXITCODE" }

    & npm run test:threading
    if ($LASTEXITCODE -ne 0) { throw "conversation threading checks failed with exit code $LASTEXITCODE" }

    & npm run test:connection-diagnostics
    if ($LASTEXITCODE -ne 0) { throw "connection diagnostic checks failed with exit code $LASTEXITCODE" }

    & npm run test:desktop-density
    if ($LASTEXITCODE -ne 0) { throw "desktop density checks failed with exit code $LASTEXITCODE" }

    & npm run test:signatures
    if ($LASTEXITCODE -ne 0) { throw "account signature checks failed with exit code $LASTEXITCODE" }

    & npm run test:link-safety
    if ($LASTEXITCODE -ne 0) { throw "external link safety checks failed with exit code $LASTEXITCODE" }

    & npm run test:html-safety
    if ($LASTEXITCODE -ne 0) { throw "HTML safety checks failed with exit code $LASTEXITCODE" }

    & npm run test:recipients
    if ($LASTEXITCODE -ne 0) { throw "recipient autocomplete checks failed with exit code $LASTEXITCODE" }

    & npm run test:undo-send
    if ($LASTEXITCODE -ne 0) { throw "undo-send checks failed with exit code $LASTEXITCODE" }

    & npm run test:schedule-send
    if ($LASTEXITCODE -ne 0) { throw "scheduled-send checks failed with exit code $LASTEXITCODE" }

    & npm run test:snooze
    if ($LASTEXITCODE -ne 0) { throw "snooze checks failed with exit code $LASTEXITCODE" }

    & npm run test:mail-rules
    if ($LASTEXITCODE -ne 0) { throw "mail rule checks failed with exit code $LASTEXITCODE" }

    & npm run test:outbox
    if ($LASTEXITCODE -ne 0) { throw "local outbox checks failed with exit code $LASTEXITCODE" }

    & npm run test:conditional-refresh
    if ($LASTEXITCODE -ne 0) { throw "conditional mailbox refresh checks failed with exit code $LASTEXITCODE" }

    & npm run test:message-hydration
    if ($LASTEXITCODE -ne 0) { throw "message hydration checks failed with exit code $LASTEXITCODE" }

    & npm run test:rich-compose
    if ($LASTEXITCODE -ne 0) { throw "rich compose checks failed with exit code $LASTEXITCODE" }

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

    Write-Host 'Publication skipped. Portable ZIPs are local-development artifacts; publish only a separately verified signed MSIX after explicit user approval.'
} finally {
    Pop-Location
}
