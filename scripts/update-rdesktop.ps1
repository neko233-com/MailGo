# MailGo uses the upstream neko233-com/rdesktop framework. Keep the local CLI current
# without coupling releases or publishing to GitHub Actions.
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$cargo = Get-Command cargo -ErrorAction Stop
$git = Get-Command git -ErrorAction Stop
$repository = 'https://github.com/neko233-com/rdesktop'
# Resolve the upstream default branch to an exact commit before invoking Cargo. This keeps the
# scheduled install on the real latest revision without handing Cargo a mutable branch name.
# Release fallback is disabled unless the deployment supplies the reviewed Authenticode
# publisher thumbprint. A digest from the same release API is not an authenticity proof.
$trustedSignerThumbprint = [string]$env:MAILGO_RDESKTOP_SIGNER_THUMBPRINT
$releaseApi = 'https://api.github.com/repos/neko233-com/rdesktop/releases/latest'

function Get-LatestRevision {
    $line = & $git.Source ls-remote $repository HEAD 2>$null | Select-Object -First 1
    if ($LASTEXITCODE -ne 0 -or !$line) {
        throw 'could not resolve the latest upstream rdesktop revision'
    }
    $match = [regex]::Match([string]$line, '(?<revision>[0-9a-fA-F]{40})\s+HEAD')
    if (!$match.Success) {
        throw 'upstream rdesktop returned an invalid revision'
    }
    return $match.Groups['revision'].Value.ToLowerInvariant()
}

function Get-InstalledRdesktop {
    $command = Get-Command rdesktop -ErrorAction SilentlyContinue
    if (!$command -or $command.CommandType -ne 'Application') {
        return $null
    }
    $versionText = (& $command.Source --version 2>$null | Select-Object -First 1)
    $match = [regex]::Match([string]$versionText, '(?<version>\d+\.\d+\.\d+)')
    $version = if ($match.Success) { [Version]$match.Groups['version'].Value } else { $null }
    [PSCustomObject]@{ Path = $command.Source; Version = $version }
}

function Try-InstallVerifiedRelease($installed) {
    try {
        $release = Invoke-RestMethod -Uri $releaseApi -Headers @{ Accept = 'application/vnd.github+json'; 'User-Agent' = 'MailGo-rdesktop-updater' } -TimeoutSec 20
        $releaseVersion = [Version]($release.tag_name -replace '^v', '')
        if ($installed -and $installed.Version -and $installed.Version -ge $releaseVersion) {
            Write-Host "Installed rdesktop $($installed.Version) is newer than or equal to latest stable release $releaseVersion; keeping it."
            return $true
        }
        $asset = $release.assets | Where-Object { $_.name -match '^rdesktop-[0-9.]+-windows-x86_64\.exe$' } | Select-Object -First 1
        if (!$asset -or !$asset.digest -or !$asset.browser_download_url) {
            return $false
        }
        if (!$installed -or !$installed.Path) {
            return $false
        }
        $temporaryPath = Join-Path ([System.IO.Path]::GetTempPath()) ("MailGo-rdesktop-{0}.exe" -f [guid]::NewGuid())
        try {
            Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $temporaryPath -UseBasicParsing -TimeoutSec 60
            $actualDigest = (Get-FileHash -LiteralPath $temporaryPath -Algorithm SHA256).Hash.ToLowerInvariant()
            $expectedDigest = ($asset.digest -replace '^sha256:', '').ToLowerInvariant()
            if ($actualDigest -ne $expectedDigest) {
                throw 'official rdesktop release checksum mismatch'
            }
            if ([string]::IsNullOrWhiteSpace($trustedSignerThumbprint)) {
                throw 'official rdesktop release fallback is disabled until a trusted signer thumbprint is configured'
            }
            $signature = Get-AuthenticodeSignature -LiteralPath $temporaryPath
            if ($signature.Status -ne 'Valid' -or !$signature.SignerCertificate) {
                throw 'official rdesktop release is not Authenticode-signed by a trusted publisher'
            }
            $actualThumbprint = ($signature.SignerCertificate.Thumbprint -replace '\s', '').ToUpperInvariant()
            $expectedThumbprint = ($trustedSignerThumbprint -replace '\s', '').ToUpperInvariant()
            if ($actualThumbprint -ne $expectedThumbprint) {
                throw 'official rdesktop release signer thumbprint mismatch'
            }
            Copy-Item -LiteralPath $temporaryPath -Destination $installed.Path -Force
            Write-Host "Installed verified rdesktop release $releaseVersion from GitHub."
            return $true
        } finally {
            Remove-Item -LiteralPath $temporaryPath -Force -ErrorAction SilentlyContinue
        }
    } catch {
        Write-Warning "Verified rdesktop release fallback unavailable: $($_.Exception.Message)"
        return $false
    }
}

Write-Host "Updating rdesktop-cli from $repository"
try {
    $latestRevision = Get-LatestRevision
    Write-Host "Installing latest upstream rdesktop revision $latestRevision"
    & $cargo.Source install rdesktop-cli --git $repository --rev $latestRevision --locked --force
    if ($LASTEXITCODE -ne 0) {
        throw "cargo install rdesktop-cli failed with exit code $LASTEXITCODE"
    }
    Write-Host 'rdesktop-cli is up to date.'
    exit 0
} catch {
    $installed = Get-InstalledRdesktop
    if (Try-InstallVerifiedRelease $installed) {
        exit 0
    }
    if ($installed) {
        Write-Warning "rdesktop-cli update deferred; preserving installed version $($installed.Version) at $($installed.Path)."
        exit 0
    }
    throw
}
