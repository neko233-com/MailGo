# MailGo uses the upstream neko233-com/rdesktop framework. Keep the local CLI current
# without coupling releases or publishing to GitHub Actions.
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$cargo = Get-Command cargo -ErrorAction Stop
$repository = 'https://github.com/neko233-com/rdesktop'
# Keep the updater on the exact rdesktop revision audited by this workspace. Move this
# trust root only as part of a reviewed dependency update; never follow an unpinned branch.
$trustedRevision = 'e9b2ba8d7a6c22138d37ca0cccfc41bbfeb28439'
$releaseApi = 'https://api.github.com/repos/neko233-com/rdesktop/releases/latest'

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
    & $cargo.Source install rdesktop-cli --git $repository --rev $trustedRevision --locked --force
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
