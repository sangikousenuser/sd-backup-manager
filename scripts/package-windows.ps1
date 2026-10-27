# Windows 用の配布物を作る（CI から呼ぶ）
#   pwsh scripts/package-windows.ps1 -Tag v0.2.0 -BinDir target\release
# 出力: dist\sd-backup-manager-setup-x64.exe（インストーラ）、dist\sd-backup-manager-<tag>-x86_64-windows.zip、dist\SHA256SUMS-windows
param(
    [Parameter(Mandatory = $true)][string]$Tag,
    [string]$BinDir = 'target\release'
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
New-Item -ItemType Directory -Force -Path dist | Out-Null

$version = $Tag.TrimStart('v')
if ($version -notmatch '^\d') { $version = '0.0.0' }

# ポータブル zip（インストーラを使わない場合・検証用）
$name = "sd-backup-manager-$Tag-x86_64-windows"
$stage = Join-Path dist $name
Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path "$stage\scripts" | Out-Null
foreach ($b in 'sd-backup-daemon.exe', 'sd-backup-gui.exe', 'sd-backup.exe') {
    Copy-Item (Join-Path $BinDir $b) $stage
}
Copy-Item config\formats.toml, README.md, LICENSE, docs\windows.md $stage
Copy-Item packaging\windows\register-task.ps1, packaging\windows\unregister-task.ps1 "$stage\scripts"
Compress-Archive -Path $stage -DestinationPath "dist\$name.zip" -Force
Remove-Item -Recurse -Force $stage

# インストーラ（Inno Setup 6）
$iscc = @(
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) {
    choco install innosetup -y --no-progress | Out-Null
    $iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
}
$src = (Resolve-Path $BinDir).Path
$out = (Resolve-Path dist).Path
& $iscc "/DAppVersion=$version" "/DSourceDir=$src" "/DOutputDir=$out" packaging\windows\installer.iss
if ($LASTEXITCODE -ne 0) { throw "ISCC failed: $LASTEXITCODE" }

# チェックサム（Linux の sha256sum と同じ書式）
$lines = foreach ($f in Get-ChildItem dist -File | Where-Object { $_.Name -like '*.exe' -or $_.Name -like '*windows.zip' }) {
    $h = (Get-FileHash $f.FullName -Algorithm SHA256).Hash.ToLower()
    "$h  $($f.Name)"
}
[System.IO.File]::WriteAllLines((Join-Path $out 'SHA256SUMS-windows'), $lines)
Get-Content dist\SHA256SUMS-windows
