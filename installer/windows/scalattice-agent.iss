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
#define MyAppUserModelId "RobottikSoftware.Scalattice.Agent"

[Setup]
AppId={{A4E8B2C1-9F3D-4A6E-8B1C-2D5E7F9A0B3C}}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/docs/providers
AppUpdatesURL={#MyAppURL}/docs/providers
VersionInfoVersion={#MyAppVersion}
VersionInfoProductName={#MyAppName}
VersionInfoProductVersion={#MyAppVersion}
VersionInfoCompany={#MyAppPublisher}
UninstallDisplayName={#MyAppName} {#MyAppVersion}
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
CloseApplications=force
CloseApplicationsFilter=scalattice-agent.exe,*.dll,*.exe

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut to open the provider dashboard"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
; Install bundled DLLs before the exe so post-install can load them.
; restartreplace: replace locked CUDA/runtime DLLs on reboot if still held briefly.
Source: "..\..\dist\lib\*"; DestDir: "{localappdata}\Scalattice\lib"; Flags: ignoreversion restartreplace recursesubdirs createallsubdirs skipifsourcedoesntexist
Source: "..\..\dist\scalattice-run.cmd"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\dist\launch-tray.vbs"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\dist\launch-tray-interactive.vbs"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\..\dist\launch-background.vbs"; DestDir: "{app}"; Flags: ignoreversion
; Prefer immediate replace of the agent exe so ARP / Explorer File version updates. Processes are stopped in CurStep/ssInstall and by silent /UPDATE.
Source: "..\..\dist\scalattice-agent.exe"; DestDir: "{app}"; Flags: ignoreversion

[InstallDelete]
Type: files; Name: "{autoprograms}\{#MyAppName} (debug).lnk"
Type: files; Name: "{app}\open-tray-debug.cmd"

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "wscript.exe"; Parameters: "//nologo ""{app}\launch-tray.vbs"""; WorkingDir: "{app}"; IconFilename: "{app}\{#MyAppExeName}"; Comment: "Open Scalattice Agent in the notification area"; AppUserModelID: "{#MyAppUserModelId}"
Name: "{autoprograms}\Scalattice Provider Dashboard"; Filename: "{#MyAppURL}/providers"; Comment: "Manage GPUs and models"
Name: "{autodesktop}\Scalattice Provider Dashboard"; Filename: "{#MyAppURL}/providers"; Tasks: desktopicon

[Code]
var
  TokenPage: TInputQueryWizardPage;
  ModelsPage: TWizardPage;
  PurgeModelsCheck: TNewCheckBox;
  PrefillToken: String;
  LibDir: String;
  ModelsCacheDir: String;
  ModelsCacheBytes: Int64;
  ShowModelsPage: Boolean;

function GetDirSize(const Dir: string; var Size: Int64): Boolean;
var
  FindRec: TFindRec;
  Path: string;
  FileSize: Int64;
begin
  Result := True;
  if not DirExists(Dir) then
    Exit;
  if FindFirst(Dir + '\*', FindRec) then
  try
    repeat
      if (FindRec.Name <> '.') and (FindRec.Name <> '..') then
      begin
        Path := Dir + '\' + FindRec.Name;
        if FindRec.Attributes and FILE_ATTRIBUTE_DIRECTORY <> 0 then
        begin
          if not GetDirSize(Path, Size) then
          begin
            Result := False;
            Exit;
          end;
        end
        else
        begin
          FileSize := FindRec.SizeLow;
          FileSize := FileSize + (Int64(FindRec.SizeHigh) shl 32);
          Size := Size + FileSize;
        end;
      end;
    until not FindNext(FindRec);
  finally
    FindClose(FindRec);
  end;
end;

function FormatCacheSize(SizeBytes: Int64): String;
var
  Gb, Mb: Double;
begin
  Gb := SizeBytes / (1024.0 * 1024.0 * 1024.0);
  if Gb >= 0.05 then
    Result := Format('%.1f GB', [Gb])
  else
  begin
    Mb := SizeBytes / (1024.0 * 1024.0);
    if Mb >= 1.0 then
      Result := Format('%.0f MB', [Mb])
    else
      Result := 'less than 1 MB';
  end;
end;

function TokenLooksValid(const Token: string): Boolean;
begin
  Result := (Length(Token) >= 16) and (Copy(Token, 1, 13) = 'slt_provider_');
end;

function ReadSavedToken(): String;
var
  TokenPath: String;
  Lines: TArrayOfString;
  I: Integer;
  Line, Value: String;
begin
  Result := '';
  TokenPath := ExpandConstant('{userpf}\.config\scalattice\agent.env');
  if not FileExists(TokenPath) then
    Exit;
  if LoadStringsFromFile(TokenPath, Lines) then
  begin
    for I := 0 to GetArrayLength(Lines) - 1 do
    begin
      Line := Trim(Lines[I]);
      if Copy(Line, 1, 23) = 'SCALATTICE_AGENT_TOKEN=' then
      begin
        Value := Trim(Copy(Line, 24, MaxInt));
        if TokenLooksValid(Value) then
          Result := Value;
        Exit;
      end;
    end;
  end;
end;

function ShouldOfferModelPurge(): Boolean;
var
  InstallDir: String;
begin
  Result := ModelsCacheBytes > 0;
  if not Result then
    Exit;
  InstallDir := ExpandConstant('{localappdata}\Scalattice\bin');
  Result := DirExists(InstallDir)
    or (ReadSavedToken() <> '')
    or FileExists(InstallDir + '\{#MyAppExeName}');
end;

procedure RemoveModelsCache;
var
  CacheRoot: String;
begin
  if not DirExists(ModelsCacheDir) then
    Exit;
  DelTree(ModelsCacheDir, True, True, True);
  CacheRoot := ExpandConstant('{userpf}\.cache\scalattice');
  if DirExists(CacheRoot) then
    RemoveDir(CacheRoot);
end;

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

procedure StopScalatticeRuntime;
var
  ResultCode: Integer;
begin
  Exec('schtasks.exe', '/End /TN ScalatticeAgent', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec('schtasks.exe', '/End /TN ScalatticeAgentTray', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec('taskkill.exe', '/IM scalattice-agent.exe /F /T', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Sleep(1500);
end;

procedure ClearReadOnlyAttributes(const Dir: string);
var
  FindRec: TFindRec;
  Path: string;
  ResultCode: Integer;
begin
  if not DirExists(Dir) then
    Exit;
  if FindFirst(Dir + '\*', FindRec) then
  try
    repeat
      if (FindRec.Name = '.') or (FindRec.Name = '..') then
        Continue;
      Path := Dir + '\' + FindRec.Name;
      if FindRec.Attributes and FILE_ATTRIBUTE_DIRECTORY <> 0 then
        ClearReadOnlyAttributes(Path)
      else if (FindRec.Attributes and FILE_ATTRIBUTE_READONLY) <> 0 then
        Exec('cmd.exe', '/c attrib -R "' + Path + '"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    until not FindNext(FindRec);
  finally
    FindClose(FindRec);
  end;
end;

function PrepareLibDirForUpgrade(const Dir: string): Boolean;
begin
  Result := True;
  if not DirExists(Dir) then
    Exit;
  ClearReadOnlyAttributes(Dir);
  if not DelTree(Dir, True, True, False) then
    Result := False;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  NeedsRestart := False;
  if not DirExists(ExpandConstant('{localappdata}\Scalattice\bin')) then
    Exit;
  StopScalatticeRuntime;
  if not PrepareLibDirForUpgrade(LibDir) then
  begin
    Result :=
      'Could not replace bundled libraries in:' + #13#10 +
      '  ' + LibDir + #13#10#13#10 +
      'The Scalattice Agent is probably still running.' + #13#10 +
      'Quit the tray from the notification area (or end scalattice-agent.exe in Task Manager), then run setup again.';
    Exit;
  end;
end;

function InitializeSetup(): Boolean;
begin
  PrefillToken := ExpandConstant('{param:TOKEN|}');
  LibDir := ExpandConstant('{localappdata}\Scalattice\lib');
  ModelsCacheDir := ExpandConstant('{userpf}\.cache\scalattice\models');
  ModelsCacheBytes := 0;
  if DirExists(ModelsCacheDir) then
    GetDirSize(ModelsCacheDir, ModelsCacheBytes);
  ShowModelsPage := ShouldOfferModelPurge();
  Result := True;
end;

procedure InitializeWizard;
var
  SizeLabel: String;
begin
  TokenPage := CreateInputQueryPage(
    wpWelcome,
    'Connect to Scalattice',
    'Paste your provider machine token',
    'Create a machine in the Scalattice Providers dashboard and paste its token below.' + #13#10 +
    'The installer saves the token, adds Scalattice to your PATH, and starts the background agent.' + #13#10 + #13#10 +
    'For NVIDIA GPUs: install a current Game Ready or Studio driver first (nvidia-smi must work).' + #13#10 +
    'You do not need the CUDA Toolkit — this installer bundles the CUDA runtime.';
  TokenPage.Add('Provider token (slt_provider_…):', False);
  if PrefillToken <> '' then
    TokenPage.Values[0] := PrefillToken
  else if ReadSavedToken() <> '' then
    TokenPage.Values[0] := ReadSavedToken();

  if ShowModelsPage then
  begin
    SizeLabel := FormatCacheSize(ModelsCacheBytes);
    ModelsPage := CreateCustomPage(
      TokenPage.ID,
      'Stored model weights',
      'A previous install left downloaded models on this PC (' + SizeLabel + ').' + #13#10 + #13#10 +
      'Keep them if you plan to run the agent again — reconnects stay instant.' + #13#10 + #13#10 +
      'Remove them only if you want to free disk space. Enabled models will download again later.');
    PurgeModelsCheck := TNewCheckBox.Create(ModelsPage);
    PurgeModelsCheck.Parent := ModelsPage.Surface;
    PurgeModelsCheck.Caption := 'Remove stored models (' + SizeLabel + ')';
    PurgeModelsCheck.Left := ScaleX(0);
    PurgeModelsCheck.Top := ScaleY(8);
    PurgeModelsCheck.Width := ModelsPage.SurfaceWidth;
    PurgeModelsCheck.Checked := False;
  end;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if CurPageID = TokenPage.ID then
  begin
    if Trim(TokenPage.Values[0]) <> '' then
    begin
      if not TokenLooksValid(Trim(TokenPage.Values[0])) then
      begin
        MsgBox('Enter a valid provider token starting with slt_provider_.' + #13#10 +
          'Create one at scalattice.cloud/providers', mbError, MB_OK);
        Result := False;
      end;
    end
    else if ReadSavedToken() <> '' then
      Exit
    else
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
  Token, SavedToken, AppDir: String;
begin
  if CurStep = ssInstall then
    StopScalatticeRuntime;

  if CurStep = ssPostInstall then
  begin
    if ShowModelsPage and (PurgeModelsCheck <> nil) and PurgeModelsCheck.Checked then
      RemoveModelsCache;

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

    SavedToken := ReadSavedToken();
    SetTokenResult := 0;
    Token := Trim(TokenPage.Values[0]);
    if Token <> '' then
    begin
      { Call the exe directly — never via .cmd — so no console window flashes. }
      Exec(AppDir + '\{#MyAppExeName}',
        'set-token --token "' + Token + '"',
        AppDir, SW_HIDE, ewWaitUntilTerminated, SetTokenResult);
    end
    else if SavedToken <> '' then
    begin
      Exec(AppDir + '\{#MyAppExeName}',
        'set-token --token "' + SavedToken + '"',
        AppDir, SW_HIDE, ewWaitUntilTerminated, SetTokenResult);
    end;

    Exec('wscript.exe', '//nologo "' + AppDir + '\launch-tray.vbs"',
      AppDir, SW_HIDE, ewNoWait, ResultCode);

    if (SavedToken = '') and (Token = '') and (SetTokenResult <> 0) then
      MsgBox('Scalattice Agent was installed, but starting the background service failed.' + #13#10 +
        'Open Command Prompt and run:' + #13#10 +
        '  scalattice-agent set-token --token YOUR_TOKEN' + #13#10 + #13#10 +
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
Type: filesandordirs; Name: "{userpf}\.config\scalattice"
