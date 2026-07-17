; Scalattice Agent - Windows GUI installer (Inno Setup 6)
; Build after dist/ is populated: scripts/build-windows-installer.ps1

#ifndef MyAppVersion
  #define MyAppVersion "1.0.0"
#endif
; Windows VERSIONINFO / ARP expect 4-part numeric versions.
#define MyVersionInfo MyAppVersion + ".0"

#define MyAppName "Scalattice Agent"
#define MyAppPublisher "Robottik Ltd"
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
VersionInfoVersion={#MyVersionInfo}
VersionInfoProductName={#MyAppName}
VersionInfoProductVersion={#MyAppVersion}
VersionInfoCompany={#MyAppPublisher}
UninstallDisplayName={#MyAppName} {#MyAppVersion}
DefaultDirName={localappdata}\Scalattice\bin
DisableDirPage=yes
DisableProgramGroupPage=yes
UsePreviousAppDir=yes
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
AppMutex=ScalatticeAgentSetup
SetupMutex=ScalatticeAgentSetupGlobal

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut to open the provider dashboard"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
; Install bundled DLLs before the exe so post-install can load them.
; PrepareToInstall moves the prior lib dir aside so we can copy fresh files without
; in-place overwriting of locked CUDA DLLs (which left stale versions / broken updates).
Source: "..\..\dist\lib\*"; DestDir: "{localappdata}\Scalattice\lib"; Flags: ignoreversion recursesubdirs createallsubdirs skipifsourcedoesntexist
Source: "..\..\dist\scalattice-run.cmd"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\dist\launch-tray.vbs"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\dist\launch-tray-interactive.vbs"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\..\dist\launch-background.vbs"; DestDir: "{app}"; Flags: ignoreversion
; Processes are stopped in PrepareToInstall / CurStep so the exe replaces immediately
; and Apps & Features / File version match MyAppVersion.
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
  IsSilentUpdate: Boolean;

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
  I: Integer;
begin
  Exec('schtasks.exe', '/End /TN ScalatticeAgent', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec('schtasks.exe', '/End /TN ScalatticeAgentTray', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  for I := 1 to 10 do
  begin
    Exec('taskkill.exe', '/IM scalattice-agent.exe /F /T', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    Sleep(400);
  end;
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

{ Move the old lib tree aside so new DLLs install into a fresh directory.
  Never hard-fail after destroying libs - that left hosts half-updated. }
procedure PrepareLibDirForUpgrade(const Dir: string);
var
  Backup: String;
  ResultCode: Integer;
begin
  if not DirExists(Dir) then
    Exit;
  ClearReadOnlyAttributes(Dir);
  Backup := Dir + '.old';
  if DirExists(Backup) then
    DelTree(Backup, True, True, True);
  if not RenameFile(Dir, Backup) then
  begin
    if not DelTree(Dir, True, True, True) then
      Exec('cmd.exe', '/c ren "' + Dir + '" "lib.old"',
        ExtractFileDir(Dir), SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end;
end;

procedure CleanupOldLibBackup;
var
  Backup: String;
begin
  Backup := LibDir + '.old';
  if DirExists(Backup) then
    DelTree(Backup, True, True, True);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  AppDir, ExePath: String;
begin
  Result := '';
  NeedsRestart := False;
  AppDir := ExpandConstant('{localappdata}\Scalattice\bin');
  if not DirExists(AppDir) and not DirExists(LibDir) then
    Exit;

  StopScalatticeRuntime;

  ExePath := AppDir + '\{#MyAppExeName}';
  if FileExists(ExePath) then
  begin
    if not DeleteFile(ExePath) then
    begin
      { Retry once after another kill burst so silent updates still replace the binary. }
      StopScalatticeRuntime;
      if not DeleteFile(ExePath) then
      begin
        Result :=
          'Could not replace scalattice-agent.exe in:' + #13#10 +
          '  ' + AppDir + #13#10#13#10 +
          'End scalattice-agent.exe in Task Manager, then run setup again.';
        Exit;
      end;
    end;
  end;

  PrepareLibDirForUpgrade(LibDir);
end;

function InitializeSetup(): Boolean;
begin
  PrefillToken := ExpandConstant('{param:TOKEN|}');
  IsSilentUpdate := ExpandConstant('{param:UPDATE|0}') = '1';
  LibDir := ExpandConstant('{localappdata}\Scalattice\lib');
  ModelsCacheDir := ExpandConstant('{userpf}\.cache\scalattice\models');
  ModelsCacheBytes := 0;
  if DirExists(ModelsCacheDir) then
    GetDirSize(ModelsCacheDir, ModelsCacheBytes);
  { Silent updates skip the cache-purge page entirely. }
  ShowModelsPage := (not WizardSilent) and (not IsSilentUpdate) and ShouldOfferModelPurge();
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
    'You do not need the CUDA Toolkit - this installer bundles the CUDA runtime.');
  TokenPage.Add('Provider token (slt_provider_...):', False);
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
      'Keep them if you plan to run the agent again - reconnects stay instant.' + #13#10 + #13#10 +
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
  if (TokenPage <> nil) and (CurPageID = TokenPage.ID) then
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

procedure LaunchScalatticeRuntime(const AppDir: String);
var
  ResultCode: Integer;
begin
  Exec('wscript.exe', '//nologo "' + AppDir + '\launch-background.vbs"',
    AppDir, SW_HIDE, ewNoWait, ResultCode);
  Sleep(800);
  Exec('wscript.exe', '//nologo "' + AppDir + '\launch-tray.vbs"',
    AppDir, SW_HIDE, ewNoWait, ResultCode);
end;

procedure StartScalatticeAfterInstall(const AppDir: String);
var
  ResultCode: Integer;
  Token, SavedToken: String;
begin
  SavedToken := ReadSavedToken();
  Token := '';
  if (TokenPage <> nil) and (not WizardSilent) and (not IsSilentUpdate) then
    Token := Trim(TokenPage.Values[0]);

  if IsSilentUpdate or WizardSilent then
  begin
    { Prefer restart so same-token set-token cannot skip relaunching tray/background. }
    Exec(AppDir + '\{#MyAppExeName}', 'restart',
      AppDir, SW_HIDE, ewWaitUntilTerminated, ResultCode);
    if ResultCode <> 0 then
    begin
      if SavedToken <> '' then
        Exec(AppDir + '\{#MyAppExeName}',
          'set-token --token "' + SavedToken + '"',
          AppDir, SW_HIDE, ewWaitUntilTerminated, ResultCode)
      else if Token <> '' then
        Exec(AppDir + '\{#MyAppExeName}',
          'set-token --token "' + Token + '"',
          AppDir, SW_HIDE, ewWaitUntilTerminated, ResultCode);
      LaunchScalatticeRuntime(AppDir);
    end
    else
    begin
      { restart usually starts both; still nudge tray/background as a safety net. }
      Sleep(1000);
      LaunchScalatticeRuntime(AppDir);
    end;
    Exit;
  end;

  if Token <> '' then
  begin
    Exec(AppDir + '\{#MyAppExeName}',
      'set-token --token "' + Token + '"',
      AppDir, SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end
  else if SavedToken <> '' then
  begin
    Exec(AppDir + '\{#MyAppExeName}',
      'set-token --token "' + SavedToken + '"',
      AppDir, SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end
  else
    ResultCode := 1;

  LaunchScalatticeRuntime(AppDir);

  if (SavedToken = '') and (Token = '') then
    MsgBox('Scalattice Agent was installed, but no provider token was found.' + #13#10 +
      'Open Command Prompt and run:' + #13#10 +
      '  scalattice-agent set-token --token YOUR_TOKEN' + #13#10 + #13#10 +
      'If you see missing cudart64_12.dll / cublas64_12.dll, the installer build did not bundle CUDA libs.' + #13#10 +
      'Check %LOCALAPPDATA%\Scalattice\lib on this machine.',
      mbInformation, MB_OK);
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  AppDir: String;
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

    StartScalatticeAfterInstall(AppDir);
    CleanupOldLibBackup;
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
