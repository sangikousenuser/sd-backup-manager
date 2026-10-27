; SD Backup Manager — Windows インストーラ（Inno Setup 6）
; CI で: ISCC.exe /DAppVersion=0.2.0 /DSourceDir=<リリースビルドのフォルダ> packaging\windows\installer.iss
; 管理者権限なし（ユーザーごと）で %LOCALAPPDATA%\Programs\SD Backup Manager にインストールする。

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\target\release"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif

[Setup]
AppId={{6E0C9B2A-5D3E-4B7A-9C1F-8A2D4E6B1C30}
AppName=SD Backup Manager
AppVersion={#AppVersion}
AppPublisher=sangikousenuser
AppPublisherURL=https://github.com/sangikousenuser/sd-backup-manager
DefaultDirName={localappdata}\Programs\SD Backup Manager
DefaultGroupName=SD Backup Manager
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=sd-backup-manager-setup-x64
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\sd-backup-gui.exe
CloseApplications=no

[Languages]
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"

[Files]
Source: "{#SourceDir}\sd-backup-daemon.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\sd-backup-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\sd-backup.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\config\formats.toml"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\..\docs\windows.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "register-task.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "unregister-task.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\SD Backup Manager"; Filename: "{app}\sd-backup-gui.exe"
Name: "{autoprograms}\SD Backup Manager 診断"; Filename: "{cmd}"; Parameters: "/k ""{app}\sd-backup.exe"" doctor"

[Registry]
; ログオン時に GUI を通常ウィンドウで表示（FR-034）
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "SDBackupManagerGUI"; ValueData: """{app}\sd-backup-gui.exe"""; Flags: uninsdeletevalue

[Run]
; ログオン時にデーモンを起動するタスクを登録し、すぐに起動する
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File ""{app}\scripts\register-task.ps1"" -InstallDir ""{app}"""; Flags: runhidden waituntilterminated; StatusMsg: "バックグラウンドサービスを登録しています..."
Filename: "{app}\sd-backup-gui.exe"; Description: "SD Backup Manager を起動する"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File ""{app}\scripts\unregister-task.ps1"" -InstallDir ""{app}"""; Flags: runhidden waituntilterminated; RunOnceId: "UnregisterTask"

[Code]
// 更新時: 動作中のデーモンを正常停止してからファイルを置き換える
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
  Script: String;
begin
  Result := '';
  Script := ExpandConstant('{app}\scripts\unregister-task.ps1');
  if FileExists(Script) then
    Exec('powershell.exe', '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "' + Script + '" -InstallDir "' + ExpandConstant('{app}') + '"',
      '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    MsgBox('アプリを削除しました。設定・履歴（%APPDATA%\sd-backup-manager, %LOCALAPPDATA%\sd-backup-manager）と ' +
      'SSD の一時保存（%USERPROFILE%\SDBackupStaging）は残しています。NAS へ未転送のファイルがないことを確認してから手動で削除してください。',
      mbInformation, MB_OK);
end;
