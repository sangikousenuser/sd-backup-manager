# SD Backup Manager: ログオン時にデーモンを起動するタスクを登録して起動する（管理者権限は不要）
# 使い方: powershell -NoProfile -ExecutionPolicy Bypass -File register-task.ps1 -InstallDir "<インストール先>"
param([Parameter(Mandatory = $true)][string]$InstallDir)
$ErrorActionPreference = 'Stop'
$TaskName = 'SD Backup Manager Daemon'
$exe = Join-Path $InstallDir 'sd-backup-daemon.exe'
$user = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name

$action = New-ScheduledTaskAction -Execute $exe -WorkingDirectory $InstallDir
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
# 異常終了時は 1 分後に再起動（systemd の Restart=on-failure 相当）。実行時間の上限なし。
$settings = New-ScheduledTaskSettingsSet `
    -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -MultipleInstances IgnoreNew -StartWhenAvailable
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited

Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
    -Settings $settings -Principal $principal -Force | Out-Null
Start-ScheduledTask -TaskName $TaskName
Write-Host "Registered and started: $TaskName"
