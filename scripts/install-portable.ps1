[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ArchivePath,
    [string]$ManifestPath,
    [string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA 'Programs\MailGo'),
    [switch]$AllowUnsignedDevelopmentBuild,
    [switch]$SkipWebView2Check,
    [switch]$CreateDesktopShortcut,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$ArchivePath = [System.IO.Path]::GetFullPath($ArchivePath)
$InstallDirectory = [System.IO.Path]::GetFullPath($InstallDirectory)
if (!(Test-Path -LiteralPath $ArchivePath -PathType Leaf)) {
    throw "portable archive does not exist: $ArchivePath"
}

if (!$ManifestPath) {
    $candidateManifest = [System.IO.Path]::ChangeExtension($ArchivePath, '.manifest.json')
    if (Test-Path -LiteralPath $candidateManifest -PathType Leaf) {
        $ManifestPath = $candidateManifest
    }
}
if (!$ManifestPath) {
    throw 'a release manifest is required; provide -ManifestPath or place the adjacent .manifest.json beside the archive'
}
$ManifestPath = [System.IO.Path]::GetFullPath($ManifestPath)
if (!(Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
    throw "release manifest does not exist: $ManifestPath"
}
$manifest = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json
$archive = Get-Item -LiteralPath $ArchivePath
if ([string]$manifest.archive -ne $archive.Name -or [int64]$manifest.bytes -ne $archive.Length) {
    throw 'portable archive does not match the release manifest name or size'
}
$actualHash = (Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash.ToUpperInvariant()
if ($actualHash -ne ([string]$manifest.sha256).ToUpperInvariant()) {
    throw 'portable archive SHA-256 does not match the release manifest'
}

function Test-WebView2Runtime {
    $roots = @(
        (Join-Path ${env:ProgramFiles(x86)} 'Microsoft\EdgeWebView\Application'),
        (Join-Path $env:ProgramFiles 'Microsoft\EdgeWebView\Application'),
        (Join-Path $env:LOCALAPPDATA 'Microsoft\EdgeWebView\Application')
    ) | Where-Object { $_ }
    foreach ($root in $roots) {
        if (Test-Path -LiteralPath $root -PathType Container) {
            $runtime = Get-ChildItem -LiteralPath $root -Filter 'msedgewebview2.exe' -File -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($runtime) { return $true }
        }
    }
    $registryRoots = @(
        'HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients',
        'HKCU:\Software\Microsoft\EdgeUpdate\Clients'
    )
    foreach ($registryRoot in $registryRoots) {
        if (Test-Path -LiteralPath $registryRoot) {
            $runtime = Get-ChildItem -LiteralPath $registryRoot -ErrorAction SilentlyContinue |
                Get-ItemProperty -ErrorAction SilentlyContinue |
                Where-Object { $_.name -like '*WebView2*' -and $_.pv }
            if ($runtime) { return $true }
        }
    }
    return $false
}

if (!$SkipWebView2Check -and !(Test-WebView2Runtime)) {
    throw 'Microsoft Edge WebView2 Evergreen Runtime is required; install it from https://developer.microsoft.com/microsoft-edge/webview2/ and retry'
}

$stagingRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("MailGo-install-{0}" -f [guid]::NewGuid())
$deploymentRoot = "$InstallDirectory.new-$([guid]::NewGuid())"
$backupDirectory = "$InstallDirectory.previous-$([DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ'))"
New-Item -ItemType Directory -Force -Path $stagingRoot | Out-Null
try {
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($ArchivePath)
    try {
        if ($zip.Entries.Count -gt 10000) { throw 'portable archive contains too many entries' }
        $uncompressedBytes = [int64]0
        foreach ($entry in $zip.Entries) {
            $normalizedName = $entry.FullName -replace '\\', '/'
            if ($normalizedName.StartsWith('/') -or $normalizedName.Contains(':') -or ($normalizedName -split '/' | Where-Object { $_ -eq '..' })) {
                throw "portable archive contains an unsafe path: $($entry.FullName)"
            }
            if ([int64]$entry.Length -gt (512MB - $uncompressedBytes)) { throw 'portable archive expands beyond the safe size limit' }
            $uncompressedBytes = $uncompressedBytes + [int64]$entry.Length
        }
    } finally {
        $zip.Dispose()
    }
    [System.IO.Compression.ZipFile]::ExtractToDirectory($ArchivePath, $stagingRoot)
    $stagedExecutable = Join-Path $stagingRoot 'MailGo.exe'
    if (!(Test-Path -LiteralPath $stagedExecutable -PathType Leaf)) { throw 'portable archive is missing MailGo.exe' }
    $header = [System.IO.File]::ReadAllBytes($stagedExecutable)
    if ($header.Length -lt 2 -or $header[0] -ne 0x4d -or $header[1] -ne 0x5a) { throw 'MailGo.exe is not a valid Windows PE file' }
    $peOffset = [System.BitConverter]::ToInt32($header, 0x3c)
    $subsystemOffset = $peOffset + 24 + 68
    if ($peOffset -lt 0 -or $subsystemOffset + 2 -gt $header.Length) { throw 'MailGo.exe has an invalid PE optional header' }
    $subsystem = [System.BitConverter]::ToUInt16($header, $subsystemOffset)
    if ($subsystem -ne 2) { throw "MailGo.exe uses PE subsystem $subsystem; refusing to install a desktop build that opens a console window" }
    $versionInfo = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($stagedExecutable)
    if ($versionInfo.ProductName -ne 'MailGo') { throw 'MailGo.exe is missing the embedded MailGo Windows resource metadata and application icon' }
    if (!$AllowUnsignedDevelopmentBuild) {
        throw 'portable ZIP installation is restricted to local source-build verification; use a verified signed MSIX for production, or pass -AllowUnsignedDevelopmentBuild for local development only'
    }
    Write-Warning 'Installing an unauthenticated local development build. Portable ZIPs must not be distributed; production installs require a verified signed MSIX.'
    if (!(Test-Path -LiteralPath (Join-Path $stagingRoot 'dist\index.html') -PathType Leaf)) { throw 'portable archive is missing the renderer' }
    if (!(Test-Path -LiteralPath (Join-Path $stagingRoot 'resources\icons\mailgo.ico') -PathType Leaf)) { throw 'portable archive is missing the tray icon' }

    if ($DryRun) {
        Write-Host "Dry run passed: archive is valid and ready for installation to $InstallDirectory"
    } else {
        $parent = Split-Path -Parent $InstallDirectory
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
        Copy-Item -LiteralPath $stagingRoot -Destination $deploymentRoot -Recurse -Force
        $iconCacheName = "mailgo-$($actualHash.Substring(0, 16).ToLowerInvariant()).ico"
        $iconCacheRelativePath = Join-Path 'resources\icons' $iconCacheName
        Copy-Item -LiteralPath (Join-Path $deploymentRoot 'resources\icons\mailgo.ico') -Destination (Join-Path $deploymentRoot $iconCacheRelativePath) -Force
        if (Test-Path -LiteralPath $InstallDirectory) {
            Move-Item -LiteralPath $InstallDirectory -Destination $backupDirectory
        }
        try {
            Move-Item -LiteralPath $deploymentRoot -Destination $InstallDirectory
        } catch {
            if ((Test-Path -LiteralPath $backupDirectory -PathType Container) -and !(Test-Path -LiteralPath $InstallDirectory)) {
                Move-Item -LiteralPath $backupDirectory -Destination $InstallDirectory
            }
            throw
        }

        $startMenu = Join-Path ([Environment]::GetFolderPath('StartMenu')) 'Programs\MailGo.lnk'
        $shell = New-Object -ComObject WScript.Shell
        $shortcut = $shell.CreateShortcut($startMenu)
        $shortcut.TargetPath = Join-Path $InstallDirectory 'MailGo.exe'
        $shortcut.WorkingDirectory = $InstallDirectory
        $shortcut.IconLocation = "$(Join-Path $InstallDirectory $iconCacheRelativePath),0"
        $shortcut.Description = 'MailGo Windows mail workspace'
        $shortcut.Save()
        if ($CreateDesktopShortcut) {
            $desktopShortcut = $shell.CreateShortcut((Join-Path ([Environment]::GetFolderPath('Desktop')) 'MailGo.lnk'))
            $desktopShortcut.TargetPath = Join-Path $InstallDirectory 'MailGo.exe'
            $desktopShortcut.WorkingDirectory = $InstallDirectory
            $desktopShortcut.IconLocation = "$(Join-Path $InstallDirectory $iconCacheRelativePath),0"
            $desktopShortcut.Description = 'MailGo Windows mail workspace'
            $desktopShortcut.Save()
        }
        Write-Host "MailGo installed to $InstallDirectory"
        if (Test-Path -LiteralPath $backupDirectory) { Write-Host "Previous version retained at $backupDirectory" }
    }
} finally {
    if (Test-Path -LiteralPath $deploymentRoot) { Remove-Item -LiteralPath $deploymentRoot -Recurse -Force -ErrorAction SilentlyContinue }
    if (Test-Path -LiteralPath $stagingRoot) { Remove-Item -LiteralPath $stagingRoot -Recurse -Force -ErrorAction SilentlyContinue }
}
