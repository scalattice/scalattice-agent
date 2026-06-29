; Scalattice Agent - Windows GUI installer (Inno Setup 6)
; Build after dist/ is populated: scripts/build-windows-installer.ps1

#ifndef MyAppVersion
  #define MyAppVersion "1.0.0"
#endif

#define MyAppName "Scalattice Agent"
#define MyAppPublisher "Robottik Software"
#define MyAppURL "https://scalattice.cloud"
#define MyAppExeName "scalattice-agent.exe"
#define MyAppId "A4E8B2C1-9F3D-4A6E-8B1C-2D5E7F9A0B3C"

[Setup]
AppId={{A4E8B2C1-9F3D-4A6E-8B1C-2D5E7F9A0B3C}}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/docs/providers
AppUpdatesURL={#MyAppURL}/docs/providers
DefaultDirName={localappdata}\Scalattice\bin
DisableDirPage=yes
DisableProgramGroupPage=yes
OutputDir=..\..\dist
OutputBaseFilename=ScalatticeAgentSetup-x86_64
SetupIconFile=scalattice.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
CloseApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut to open the provider dashboard"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
; Install bundled DLLs before the exe so CloseApplications / post-install can load them.
Source: "..\..\dist\lib\*"; DestDir: "{localappdata}\Scalattice\lib"; Flags: ignoreversion recursesubdirs createallsubdirs skipifsourcedoesntexist
Source: "..\..\dist\scalattice-run.cmd"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\dist\launch-tray.vbs"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\dist\launch-background.vbs"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\dist\scalattice-agent.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\launch-tray.vbs"; Comment: "Open status, token, and live log panel"
Name: "{autoprograms}\Scalattice Provider Dashboard"; Filename: "{#MyAppURL}/providers"; Comment: "Manage GPUs and models"
Name: "{autodesktop}\Scalattice Provider Dashboard"; Filename: "{#MyAppURL}/providers"; Tasks: desktopicon

[Code]
var
  TokenPage: TInputQueryWizardPage;
  PrefillToken: String;
  LibDir: String;

function NeedsAddPath(Param: string): Boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', OrigPath) then
    OrigPath := '';
  Result := Pos(';' + UpperCase(Param) + ';', ';' + UpperCase(OrigPath) + ';') = 0;
end;

procedure AddToUserPath(PathToAdd: string);
var
  OrigPath, NewPath: string;
begin
  if RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', OrigPath) then
    NewPath := PathToAdd + ';' + OrigPath
  else
    NewPath := PathToAdd;
  RegWriteStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', NewPath);
end;

procedure BroadcastEnvironmentChange;
var
  Msg: LongWord;
begin
  Msg := $001A;
  SendNotifyMessage(HWND_BROADCAST, Msg, 0, 0);
end;

function TokenLooksValid(const Token: string): Boolean;
begin
  Result := (Length(Token) >= 16) and (Copy(Token, 1, 13) = 'slt_provider_');
end;

function InitializeSetup(): Boolean;
begin
  PrefillToken := ExpandConstant('{param:TOKEN|}');
  LibDir := ExpandConstant('{localappdata}\Scalattice\lib');
  Result := True;
end;

procedure InitializeWizard;
begin
  TokenPage := CreateInputQueryPage(
    wpWelcome,
    'Connect to Scalattice',
    'Paste your provider machine token',
    'Create a machine in the Scalattice Providers dashboard and paste its token below.' + #13#10 +
    'The installer saves the token, adds Scalattice to your PATH, and starts the background agent.');
  TokenPage.Add('Provider token (slt_provider_…):', False);
  if PrefillToken <> '' then
    TokenPage.Values[0] := PrefillToken;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if CurPageID = TokenPage.ID then
  begin
    if not TokenLooksValid(Trim(TokenPage.Values[0])) then
    begin
      MsgBox('Enter a valid provider token starting with slt_provider_.' + #13#10 +
        'Create one at scalattice.cloud/providers', mbError, MB_OK);
      Result := False;
    end;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  ResultCode: Integer;
  SetTokenResult: Integer;
  Token, AppDir: String;
begin
  if CurStep = ssPostInstall then
  begin
    AppDir := ExpandConstant('{app}');
    if NeedsAddPath(AppDir) then
    begin
      AddToUserPath(AppDir);
      BroadcastEnvironmentChange;
    end;
    if NeedsAddPath(LibDir) then
    begin
      AddToUserPath(LibDir);
      BroadcastEnvironmentChange;
    end;

    Token := Trim(TokenPage.Values[0]);
    Exec(AppDir + '\scalattice-run.cmd',
      'set-token --token "' + Token + '"',
      AppDir, SW_HIDE, ewWaitUntilTerminated, SetTokenResult);

    Exec('wscript.exe', '//nologo "' + AppDir + '\launch-tray.vbs"',
      AppDir, SW_HIDE, ewNoWait, ResultCode);

    if SetTokenResult <> 0 then
      MsgBox('Scalattice Agent was installed, but starting the background service failed.' + #13#10 +
        'Open Command Prompt and run:' + #13#10 +
        '  scalattice-run.cmd set-token --token YOUR_TOKEN' + #13#10 + #13#10 +
        'If you see missing cudart64_12.dll / cublas64_12.dll, the installer build did not bundle CUDA libs.' + #13#10 +
        'Check %LOCALAPPDATA%\Scalattice\lib on this machine.',
        mbInformation, MB_OK);
  end;
end;

[UninstallRun]
Filename: "{app}\scalattice-run.cmd"; Parameters: "uninstall --yes"; Flags: runhidden waituntilterminated skipifdoesntexist

[UninstallDelete]
Type: files; Name: "{localappdata}\Scalattice\lib\*"
Type: dirifempty; Name: "{localappdata}\Scalattice\lib"
Type: files; Name: "{app}\run-background.cmd"
Type: dirifempty; Name: "{localappdata}\Scalattice\bin"
Type: dirifempty; Name: "{localappdata}\Scalattice\logs"
Type: dirifempty; Name: "{localappdata}\Scalattice"
Type: filesandordirs; Name: "{%USERPROFILE}\.config\scalattice"
