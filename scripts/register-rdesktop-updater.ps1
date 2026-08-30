# Registers a per-user weekly updater. It only installs the upstream rdesktop CLI;
# publishing and release actions remain explicit, manual operations.
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$taskName = 'MailGo-rdesktop-updater'
$scriptPath = (Resolve-Path (Join-Path $PSScriptRoot 'update-rdesktop.ps1')).Path
$argument = "-NoProfile -ExecutionPolicy Bypass -File `"$scriptPath`""
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument $argument
$trigger = New-ScheduledTaskTrigger -Weekly -DaysOfWeek Sunday -At 3:00am
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -MultipleInstances IgnoreNew

Register-ScheduledTask `
    -TaskName $taskName `
    -Action $action `
    -Trigger $trigger `
    -Settings $settings `
    -Description 'Keep the MailGo rdesktop framework CLI current; does not publish releases.' `
    -Force | Out-Null

Write-Host "Registered scheduled task: $taskName"
