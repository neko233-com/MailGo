[CmdletBinding()]
param(
    [string]$OutputDirectory,
    [Parameter(Mandatory = $true)]
    [string]$Publisher,
    [string]$CertificatePath,
    [string]$CertificateThumbprint,
    [string]$TimestampUrl = 'https://timestamp.digicert.com'
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'windows-icon-assets.ps1')
$OutputDirectory = if ($OutputDirectory) {
    [System.IO.Path]::GetFullPath($OutputDirectory)
} else {
    Join-Path $projectRoot 'artifacts'
}
$package = Get-Content (Join-Path $projectRoot 'package.json') -Raw | ConvertFrom-Json
$version = [string]$package.version
$appxVersion = ($version -split '\.') + @('0', '0', '0')
$appxVersion = ($appxVersion[0..3] -join '.')
$packageName = "MailGo-$version-windows-x64.msix"
$packagePath = Join-Path $OutputDirectory $packageName
$portableOutput = Join-Path $OutputDirectory '.portable-msix-input'
$stageRoot = Join-Path $OutputDirectory ".MailGo-$version-msix-stage"
$makeAppx = Get-Command makeappx.exe -ErrorAction SilentlyContinue
$signTool = Get-Command signtool.exe -ErrorAction SilentlyContinue

if (!$makeAppx) { throw 'makeappx.exe is required; install the Windows SDK on the release host' }
if (!$signTool) { throw 'signtool.exe is required; install the Windows SDK on the release host' }
if ([string]::IsNullOrWhiteSpace($Publisher)) { throw 'Publisher must match the signing certificate subject' }
if ([string]::IsNullOrWhiteSpace($CertificatePath) -and [string]::IsNullOrWhiteSpace($CertificateThumbprint)) {
    throw 'provide either -CertificatePath or -CertificateThumbprint for a production signature'
}
if ($CertificatePath -and $CertificateThumbprint) {
    throw 'provide only one certificate source'
}
if ($CertificatePath -and !(Test-Path -LiteralPath $CertificatePath -PathType Leaf)) {
    throw "signing certificate does not exist: $CertificatePath"
}

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
if (Test-Path -LiteralPath $stageRoot) { Remove-Item -LiteralPath $stageRoot -Recurse -Force }
if (Test-Path -LiteralPath $packagePath) { Remove-Item -LiteralPath $packagePath -Force }

# Reuse the same deterministic, Release-built portable input as the ZIP gate. This command also
# verifies that the renderer and native executable are built from the current checkout.
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $projectRoot 'scripts\package-windows.ps1') -OutputDirectory $portableOutput
if ($LASTEXITCODE -ne 0) { throw "portable input build failed with exit code $LASTEXITCODE" }
$portableStage = Join-Path $portableOutput "MailGo-$version-windows-x64"
if (!(Test-Path -LiteralPath $portableStage -PathType Container)) {
    throw "portable staging directory is missing: $portableStage"
}
Copy-Item -LiteralPath $portableStage -Destination $stageRoot -Recurse -Force

$manifestPath = Join-Path $stageRoot 'AppxManifest.xml'
$assetsPath = Join-Path $stageRoot 'Assets'
New-MailGoMsixAssets -SourcePath (Join-Path $projectRoot 'resources\icons\mailgo-source.png') -DestinationDirectory $assetsPath
$escapedPublisher = [System.Security.SecurityElement]::Escape($Publisher)
$manifest = @"
<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10" xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10" xmlns:rescap="http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities" IgnorableNamespaces="uap rescap">
  <Identity Name="com.neko233.MailGo" Publisher="$escapedPublisher" Version="$appxVersion" ProcessorArchitecture="x64" />
  <Properties>
    <DisplayName>MailGo</DisplayName>
    <PublisherDisplayName>neko233-com</PublisherDisplayName>
    <Description>Local-first Windows mail workspace</Description>
    <Logo>Assets\StoreLogo.png</Logo>
  </Properties>
  <Resources>
    <Resource Language="zh-CN" />
  </Resources>
  <Applications>
    <Application Id="MailGo" Executable="MailGo.exe" EntryPoint="Windows.FullTrustApplication">
      <uap:VisualElements AppListEntry="default" DisplayName="MailGo" Description="Local-first Windows mail workspace" Square150x150Logo="Assets\Square150x150Logo.png" Square44x44Logo="Assets\Square44x44Logo.png" BackgroundColor="transparent" />
    </Application>
  </Applications>
  <Capabilities>
    <rescap:Capability Name="runFullTrust" />
  </Capabilities>
</Package>
"@
Set-Content -LiteralPath $manifestPath -Value $manifest -Encoding utf8

& $makeAppx.Source pack /d $stageRoot /p $packagePath /o
if ($LASTEXITCODE -ne 0) { throw "makeappx pack failed with exit code $LASTEXITCODE" }

$signArguments = @('sign', '/fd', 'SHA256', '/tr', $TimestampUrl, '/td', 'SHA256')
if ($CertificateThumbprint) {
    $signArguments += @('/sha1', ($CertificateThumbprint -replace '\s', ''))
} else {
    $pfxPassword = [string]$env:MAILGO_SIGNING_PFX_PASSWORD
    if (!$env:MAILGO_SIGNING_PFX_PASSWORD) {
        throw 'MAILGO_SIGNING_PFX_PASSWORD must be set when -CertificatePath is used'
    }
    $signArguments += @('/f', (Resolve-Path -LiteralPath $CertificatePath).Path, '/p', $pfxPassword)
}
$signArguments += $packagePath
& $signTool.Source @signArguments
if ($LASTEXITCODE -ne 0) { throw "signtool signing failed with exit code $LASTEXITCODE" }

# MSIX carries an AppX package signature rather than a PE signature, so verify it with the same
# Windows SDK toolchain that produced it. Verbose verification must include the requested
# publisher subject; a successful signature from an unrelated certificate is not acceptable.
$verificationOutput = & $signTool.Source verify /pa /all /v $packagePath 2>&1
$verificationExitCode = $LASTEXITCODE
$verificationText = ($verificationOutput | Out-String)
if ($verificationExitCode -ne 0) {
    throw "MSIX signature verification failed: $verificationText"
}
if ($verificationText -notlike "*$Publisher*") {
    throw 'MSIX signer subject does not match the requested Publisher'
}
Write-Host "Signed MSIX created: $packagePath ($((Get-Item -LiteralPath $packagePath).Length) bytes)"
Write-Host "Verified publisher: $Publisher"
