[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$projectRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'windows-icon-assets.ps1')

function Get-BigEndianUInt32([byte[]]$Bytes, [int]$Offset) {
    return ([uint32]$Bytes[$Offset] -shl 24) -bor
        ([uint32]$Bytes[$Offset + 1] -shl 16) -bor
        ([uint32]$Bytes[$Offset + 2] -shl 8) -bor
        [uint32]$Bytes[$Offset + 3]
}

function Assert-Png([string]$Path, [int]$ExpectedSize) {
    if (!(Test-Path -LiteralPath $Path -PathType Leaf)) { throw "missing PNG asset: $Path" }
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 26 -or
        $bytes[0] -ne 0x89 -or $bytes[1] -ne 0x50 -or $bytes[2] -ne 0x4e -or $bytes[3] -ne 0x47 -or
        $bytes[12] -ne 0x49 -or $bytes[13] -ne 0x48 -or $bytes[14] -ne 0x44 -or $bytes[15] -ne 0x52) {
        throw "invalid PNG asset: $Path"
    }
    $width = Get-BigEndianUInt32 $bytes 16
    $height = Get-BigEndianUInt32 $bytes 20
    if ($width -ne $ExpectedSize -or $height -ne $ExpectedSize) {
        throw "PNG $Path is ${width}x${height}; expected ${ExpectedSize}x${ExpectedSize}"
    }
    if ($bytes[24] -ne 8 -or $bytes[25] -ne 6) {
        throw "PNG $Path must use 8-bit RGBA color"
    }
}

function Assert-Ico([string]$Path, [int[]]$ExpectedSizes) {
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 6 -or [BitConverter]::ToUInt16($bytes, 0) -ne 0 -or [BitConverter]::ToUInt16($bytes, 2) -ne 1) {
        throw "invalid ICO header: $Path"
    }
    $count = [BitConverter]::ToUInt16($bytes, 4)
    if ($count -ne $ExpectedSizes.Count) { throw "ICO contains $count entries; expected $($ExpectedSizes.Count)" }
    $actualSizes = [System.Collections.Generic.HashSet[int]]::new()
    for ($index = 0; $index -lt $count; $index++) {
        $entry = 6 + (16 * $index)
        $width = if ($bytes[$entry] -eq 0) { 256 } else { [int]$bytes[$entry] }
        $height = if ($bytes[$entry + 1] -eq 0) { 256 } else { [int]$bytes[$entry + 1] }
        $bits = [BitConverter]::ToUInt16($bytes, $entry + 6)
        $length = [BitConverter]::ToUInt32($bytes, $entry + 8)
        $offset = [BitConverter]::ToUInt32($bytes, $entry + 12)
        if ($width -ne $height -or $bits -ne 32 -or $length -lt 26 -or [uint64]$offset + [uint64]$length -gt [uint64]$bytes.Length) {
            throw "invalid ${width}x${height} ICO entry"
        }
        if ($bytes[$offset] -ne 0x89 -or $bytes[$offset + 1] -ne 0x50 -or $bytes[$offset + 2] -ne 0x4e -or $bytes[$offset + 3] -ne 0x47) {
            throw "${width}x${height} ICO entry is not PNG encoded"
        }
        [void]$actualSizes.Add($width)
    }
    foreach ($size in $ExpectedSizes) {
        if (!$actualSizes.Contains($size)) { throw "ICO is missing ${size}x${size}" }
    }
}

$temporaryRoot = Join-Path ([System.IO.Path]::GetTempPath()) "MailGo-icon-test-$([Guid]::NewGuid().ToString('N'))"
$resolvedTempBase = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
$resolvedTemporaryRoot = [System.IO.Path]::GetFullPath($temporaryRoot)
if (!$resolvedTemporaryRoot.StartsWith($resolvedTempBase, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw 'temporary icon test path escaped the system temporary directory'
}

$sizes = @(16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 128, 256)
$sourcePath = Join-Path $projectRoot 'resources\icons\mailgo-source.png'
try {
    $generatedCore = Join-Path $temporaryRoot 'core'
    $generatedMsix = Join-Path $temporaryRoot 'msix'
    New-MailGoCoreIconSet $sourcePath $generatedCore
    New-MailGoMsixAssets $sourcePath $generatedMsix

    foreach ($size in $sizes) {
        Assert-Png (Join-Path $generatedCore "mailgo-$size.png") $size
        Assert-Png (Join-Path $projectRoot "resources\icons\mailgo-$size.png") $size
    }
    Assert-Ico (Join-Path $generatedCore 'mailgo.ico') $sizes
    Assert-Ico (Join-Path $projectRoot 'resources\icons\mailgo.ico') $sizes

    foreach ($asset in @(
        @{ Name = 'StoreLogo.png'; Size = 50 },
        @{ Name = 'Square44x44Logo.png'; Size = 44 },
        @{ Name = 'Square150x150Logo.png'; Size = 150 }
    )) {
        Assert-Png (Join-Path $generatedMsix $asset.Name) $asset.Size
    }
    foreach ($specification in @(
        @{ Stem = 'StoreLogo'; Base = 50 },
        @{ Stem = 'Square44x44Logo'; Base = 44 },
        @{ Stem = 'Square150x150Logo'; Base = 150 }
    )) {
        foreach ($scale in 100, 200, 400) {
            Assert-Png (Join-Path $generatedMsix "$($specification.Stem).scale-$scale.png") ([int]($specification.Base * $scale / 100))
        }
    }
    foreach ($size in 16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 256) {
        foreach ($suffix in '.png', '_altform-unplated.png', '_altform-lightunplated.png') {
            Assert-Png (Join-Path $generatedMsix "Square44x44Logo.targetsize-$size$suffix") $size
        }
    }

    $msixScript = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'package-msix.ps1') -Raw
    if (!$msixScript.Contains('New-MailGoMsixAssets')) { throw 'MSIX packaging does not generate qualified Windows icon assets' }
    if ($msixScript -match "Copy-Item.+mailgo-(256|48)\.png") { throw 'MSIX packaging still renames incorrectly sized PNG assets' }
    if (!$msixScript.Contains('BackgroundColor="transparent"')) { throw 'MSIX visual elements must preserve the transparent icon silhouette' }

    $portableScript = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'package-windows.ps1') -Raw
    if (!$portableScript.Contains('ExtractAssociatedIcon')) { throw 'portable packaging must verify the embedded executable icon' }

    $traySource = Get-Content -LiteralPath (Join-Path $projectRoot 'native\src\tray.rs') -Raw
    if (!$traySource.Contains('GetSystemMetricsForDpi')) { throw 'tray icon loading must use the current Windows DPI' }
    Write-Host 'Windows icon checks passed: exact ICO sizes, MSIX scale assets, target-size theme variants, and DPI-aware tray loading.'
} finally {
    if (Test-Path -LiteralPath $resolvedTemporaryRoot) {
        Remove-Item -LiteralPath $resolvedTemporaryRoot -Recurse -Force
    }
}
