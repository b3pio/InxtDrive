; InxtDrive installer (Inno Setup 6). Build with scripts\build-installer.ps1.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#define WinFspMsi "winfsp-2.1.25156.msi"

[Setup]
AppId={{9C3B4F1A-6E2D-4B8A-A1F7-3D5E8C2B9A40}
AppName=InxtDrive
AppVersion={#AppVersion}
AppVerName=InxtDrive {#AppVersion}
AppPublisher=InxtDrive contributors
AppComments=Unofficial Internxt client. Not affiliated with Internxt.
DefaultDirName={autopf}\InxtDrive
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
OutputDir=..\dist
OutputBaseFilename=InxtDrive-Setup-{#AppVersion}
SetupIconFile=..\assets\inxtdrive.ico
UninstallDisplayIcon={app}\inxtdrive.exe
UninstallDisplayName=InxtDrive
LicenseFile=..\LICENSE
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
CloseApplications=no
RestartApplications=no
; Per-user state (autostart, Explorer entries) is written by the app itself,
; which runs as the signed-in user; uninstall only cleans up after it.
UsedUserAreasWarning=no
VersionInfoVersion={#AppVersion}
VersionInfoDescription=InxtDrive Setup

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "..\target\release\inxtdrive.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\inxt.exe"; DestDir: "{app}"; Flags: ignoreversion
; Explorer keeps the badge DLL loaded; a locked copy is swapped at the next restart.
Source: "..\target\release\inxt_overlay.dll"; DestDir: "{app}"; Flags: ignoreversion restartreplace uninsrestartdelete
Source: "..\assets\overlay-synced.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\assets\overlay-syncing.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "redist\{#WinFspMsi}"; DestDir: "{tmp}"; Flags: deleteafterinstall; Check: NeedsWinFsp

[Icons]
Name: "{autoprograms}\InxtDrive"; Filename: "{app}\inxtdrive.exe"
Name: "{autodesktop}\InxtDrive"; Filename: "{app}\inxtdrive.exe"; Tasks: desktopicon

[Run]
Filename: "msiexec.exe"; Parameters: "/i ""{tmp}\{#WinFspMsi}"" /passive /norestart"; StatusMsg: "Installing WinFsp (the file system driver InxtDrive uses)..."; Check: NeedsWinFsp; Flags: waituntilterminated
Filename: "{app}\inxtdrive.exe"; Parameters: "--register-overlays"; StatusMsg: "Registering Explorer status icons..."; Flags: runhidden waituntilterminated
Filename: "{cmd}"; Parameters: "/c taskkill /f /im explorer.exe & start """" explorer.exe"; Description: "Restart Windows Explorer now so status icons appear (closes open Explorer windows)"; Flags: postinstall unchecked skipifsilent runhidden runasoriginaluser
Filename: "{app}\inxtdrive.exe"; Description: "Start InxtDrive now"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
Filename: "{app}\inxt.exe"; Parameters: "quit"; Flags: runhidden waituntilterminated; RunOnceId: "QuitApp"
Filename: "{app}\inxtdrive.exe"; Parameters: "--cleanup"; Flags: runhidden waituntilterminated; RunOnceId: "Cleanup"
Filename: "{app}\inxtdrive.exe"; Parameters: "--unregister-overlays"; Flags: runhidden waituntilterminated; RunOnceId: "Overlays"

[Code]
function WinFspOk(): Boolean;
var
  Dir: String;
  MS, LS: Cardinal;
begin
  Result := False;
  if RegQueryStringValue(HKLM32, 'SOFTWARE\WinFsp', 'InstallDir', Dir) then
    if GetVersionNumbers(AddBackslash(Dir) + 'bin\winfsp-x64.dll', MS, LS) then
      Result := (MS shr 16) >= 2;
end;

function NeedsWinFsp(): Boolean;
begin
  Result := not WinFspOk();
end;

function AppRunning(): Boolean;
var
  Code: Integer;
begin
  Result := Exec(ExpandConstant('{cmd}'), '/C tasklist /FI "IMAGENAME eq inxtdrive.exe" /NH | find /I "inxtdrive.exe" >NUL',
    '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0);
end;

{ Ask a running copy to unmount and exit cleanly before its files are replaced. }
procedure StopRunningApp();
var
  Code, I: Integer;
begin
  if not AppRunning() then
    exit;
  if FileExists(ExpandConstant('{app}\inxt.exe')) then
    Exec(ExpandConstant('{app}\inxt.exe'), 'quit', '', SW_HIDE, ewWaitUntilTerminated, Code);
  for I := 1 to 30 do
  begin
    if not AppRunning() then
      exit;
    Sleep(500);
  end;
  Exec('taskkill.exe', '/IM inxtdrive.exe /F', '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopRunningApp();
  Result := '';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    StopRunningApp();
  if CurUninstallStep = usPostUninstall then
    if not UninstallSilent() then
      if MsgBox('Also delete InxtDrive''s data on this PC (settings, sign-in and the local copy of your files)?' + #13#10#13#10 +
                'Files that finished uploading stay in your Internxt account either way.',
                mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
      begin
        DelTree(ExpandConstant('{localappdata}\InxtDrive'), True, True, True);
        DelTree(ExpandConstant('{userappdata}\InxtDrive'), True, True, True);
      end;
end;
