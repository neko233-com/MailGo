# MailGo uses the upstream neko233-com/rdesktop framework. Keep the local CLI current
# without coupling releases or publishing to GitHub Actions.
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$cargo = Get-Command cargo -ErrorAction Stop
$repository = 'https://github.com/neko233-com/rdesktop'

Write-Host "Updating rdesktop-cli from $repository"
& $cargo.Source install rdesktop-cli --git $repository --locked --force
if ($LASTEXITCODE -ne 0) {
    throw "cargo install rdesktop-cli failed with exit code $LASTEXITCODE"
}

Write-Host 'rdesktop-cli is up to date.'
