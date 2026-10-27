# SD Backup Manager: デーモンを正常停止し、ログオン時の起動タスクを削除する
param([string]$InstallDir = '')
$TaskName = 'SD Backup Manager Daemon'
if ($InstallDir -ne '') {
    $cli = Join-Path $InstallDir 'sd-backup.exe'
    if (Test-Path $cli) {
        # 処理中のコピーは安全に中断され、次回起動時に再開する
        & $cli shutdown 2>$null | Out-Null
        for ($i = 0; $i -lt 30; $i++) {
            if (-not (Get-Process -Name 'sd-backup-daemon' -ErrorAction SilentlyContinue)) { break }
            Start-Sleep -Milliseconds 500
        }
    }
}
Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
# 正常停止しなかった場合のみ強制終了（その場合も起動時の復旧で安全にやり直せる）
Get-Process -Name 'sd-backup-daemon', 'sd-backup-gui' -ErrorAction SilentlyContinue | Stop-Process -Force
Write-Host "Unregistered: $TaskName"
