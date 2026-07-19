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

; Fail the installer compile if the release bundle omitted CUDA runtime DLLs.
; (skipifsourcedoesntexist used to ship a broken setup.exe with an empty lib folder.)
#if !FileExists("..\..\dist\lib\cudart64_12.dll")
  #error dist\lib\cudart64_12.dll missing — run scripts\bundle-release-windows.ps1 before building the installer
#endif
#if !FileExists("..\..\dist\lib\cublas64_12.dll")
  #error dist\lib\cublas64_12.dll missing — run scripts\bundle-release-windows.ps1 before building the installer
#endif
#if !FileExists("..\..\dist\lib\cublasLt64_12.dll")
  #error dist\lib\cublasLt64_12.dll missing — run scripts\bundle-release-windows.ps1 before building the installer
#endif

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

[Files]
; Extracted to {tmp} at runtime for the Compatible devices wizard page.
Source: "detect-compute-devices.ps1"; Flags: dontcopy
Source: "resolve-nvidia-driver.ps1"; Flags: dontcopy
; Install bundled DLLs before the exe so post-install can load them.
; PrepareToInstall moves the prior lib dir aside so we can copy fresh files without
; in-place overwriting of locked CUDA DLLs (which left stale versions / broken updates).
Source: "..\..\dist\lib\*"; DestDir: "{localappdata}\Scalattice\lib"; Flags: ignoreversion recursesubdirs createallsubdirs
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

[Code]
var
  DevicesPage: TWizardPage;
  DevicesMemo: TNewMemo;
  DriverStatusLabel: TNewStaticText;
  DriverDownloadBtn: TNewButton;
  DriverRecheckBtn: TNewButton;
  TokenPage: TInputQueryWizardPage;
  ModelsPage: TWizardPage;
  PurgeModelsCheck: TNewCheckBox;
  PrefillToken: String;
  LibDir: String;
  ModelsCacheDir: String;
  ModelsCacheBytes: Int64;
  ShowModelsPage: Boolean;
  ShowDevicesPage: Boolean;
  IsSilentUpdate: Boolean;
  ResolvedDriverUrl: String;
  ResolvedGpuName: String;
  ResolvedDriverVersion: String;
  DriverLookupDone: Boolean;
  InventoryNvidiaPresent: Boolean;
  InventoryNvidiaSmiOk: Boolean;


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

function CudaRuntimePresent(const Dir: String): Boolean;
begin
  Result :=
    FileExists(Dir + '\cudart64_12.dll') and
    FileExists(Dir + '\cublas64_12.dll') and
    FileExists(Dir + '\cublasLt64_12.dll');
end;

{ True when nvidia-smi can list GPUs (driver installed and working). }
function NvidiaDriverOk: Boolean;
var
  Candidates: array[0..3] of String;
  I, ResultCode: Integer;
begin
  Result := False;
  Candidates[0] := ExpandConstant('{win}\System32\nvidia-smi.exe');
  Candidates[1] := ExpandConstant('{pf}\NVIDIA Corporation\NVSMI\nvidia-smi.exe');
  Candidates[2] := ExpandConstant('{pf32}\NVIDIA Corporation\NVSMI\nvidia-smi.exe');
  Candidates[3] := 'nvidia-smi.exe';
  for I := 0 to 3 do
  begin
    if (I < 3) and (not FileExists(Candidates[I])) then
      Continue;
    if Exec(Candidates[I], '-L', '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    begin
      if ResultCode = 0 then
      begin
        Result := True;
        Exit;
      end;
    end;
  end;
end;

function DriverLookupIniPath: String;
begin
  Result := ExpandConstant('{tmp}\scalattice-nvidia-driver.ini');
end;

function InventoryIniPath: String;
begin
  Result := ExpandConstant('{tmp}\scalattice-compute-inventory.ini');
end;

procedure EnsureDriverLookupScript;
begin
  if not FileExists(ExpandConstant('{tmp}\resolve-nvidia-driver.ps1')) then
    ExtractTemporaryFile('resolve-nvidia-driver.ps1');
end;

procedure EnsureInventoryScript;
begin
  if not FileExists(ExpandConstant('{tmp}\detect-compute-devices.ps1')) then
    ExtractTemporaryFile('detect-compute-devices.ps1');
end;

procedure RunComputeInventory;
var
  ScriptPath, IniPath, Params: String;
  ResultCode: Integer;
begin
  InventoryNvidiaPresent := False;
  InventoryNvidiaSmiOk := False;
  EnsureInventoryScript;
  ScriptPath := ExpandConstant('{tmp}\detect-compute-devices.ps1');
  IniPath := InventoryIniPath;
  DeleteFile(IniPath);
  Params :=
    '-NoProfile -ExecutionPolicy Bypass -File "' + ScriptPath + '" -OutFile "' + IniPath + '"';
  if not Exec('powershell.exe', Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Exit;
  if not FileExists(IniPath) then
    Exit;
  InventoryNvidiaPresent := GetIniString('Inventory', 'NvidiaPresent', '0', IniPath) = '1';
  InventoryNvidiaSmiOk := GetIniString('Inventory', 'NvidiaSmiOk', '0', IniPath) = '1';
end;

function FormatGpuKind(const Kind, Vendor: String): String;
begin
  if Kind = 'integrated' then
    Result := 'integrated GPU'
  else if Vendor = 'nvidia' then
    Result := 'NVIDIA GPU'
  else if Vendor = 'amd' then
    Result := 'AMD GPU'
  else if Vendor = 'intel' then
    Result := 'Intel GPU'
  else
    Result := 'GPU';
end;

function BuildInventoryCaption: String;
var
  IniPath, CpuName, Name, Kind, Vendor, VramMb, Line: String;
  GpuCount, I, Vram: Integer;
begin
  IniPath := InventoryIniPath;
  if not FileExists(IniPath) then
  begin
    Result :=
      'Could not scan this PC for compute devices.' + #13#10 +
      'You can continue; the agent will detect hardware after install.';
    Exit;
  end;

  CpuName := GetIniString('Inventory', 'CpuName', 'CPU', IniPath);
  GpuCount := GetIniInt('Inventory', 'GpuCount', 0, 0, 64, IniPath);
  Result := 'CPU' + #13#10 + '  ' + CpuName + #13#10;

  if GpuCount <= 0 then
    Result := Result + #13#10 + 'GPUs' + #13#10 + '  None detected'
  else
  begin
    Result := Result + #13#10 + 'GPUs';
    for I := 0 to GpuCount - 1 do
    begin
      Name := GetIniString('Gpu' + IntToStr(I), 'Name', 'Unknown GPU', IniPath);
      Kind := GetIniString('Gpu' + IntToStr(I), 'Kind', 'discrete', IniPath);
      Vendor := GetIniString('Gpu' + IntToStr(I), 'Vendor', 'other', IniPath);
      VramMb := GetIniString('Gpu' + IntToStr(I), 'VramMb', '0', IniPath);
      Line := '  - ' + Name + '  (' + FormatGpuKind(Kind, Vendor) + ')';
      Vram := StrToIntDef(VramMb, 0);
      { VRAM comes from registry qwMemorySize / dxdiag — never WMI AdapterRAM. }
      if Vram >= 1024 then
        Line := Line + '  · ' + IntToStr((Vram + 512) div 1024) + ' GB VRAM'
      else if Vram > 0 then
        Line := Line + '  · ' + IntToStr(Vram) + ' MB VRAM';
      Result := Result + #13#10 + Line;
    end;
  end;
end;

procedure RunNvidiaDriverLookup;
var
  ScriptPath, IniPath, Params: String;
  ResultCode: Integer;
begin
  ResolvedDriverUrl := '';
  ResolvedGpuName := '';
  ResolvedDriverVersion := '';
  DriverLookupDone := False;

  EnsureDriverLookupScript;
  ScriptPath := ExpandConstant('{tmp}\resolve-nvidia-driver.ps1');
  IniPath := DriverLookupIniPath;
  DeleteFile(IniPath);

  Params :=
    '-NoProfile -ExecutionPolicy Bypass -File "' + ScriptPath + '" -OutFile "' + IniPath + '"';
  if not Exec('powershell.exe', Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Exit;

  if not FileExists(IniPath) then
    Exit;

  ResolvedGpuName := GetIniString('Driver', 'GpuName', '', IniPath);
  ResolvedDriverVersion := GetIniString('Driver', 'Version', '', IniPath);
  ResolvedDriverUrl := GetIniString('Driver', 'DownloadUrl', '', IniPath);
  DriverLookupDone := True;
end;

procedure RefreshDevicesStatus;
var
  Err: String;
  Laptop: String;
  InstalledVer: String;
  DriverReady: Boolean;
begin
  RunComputeInventory;

  if DevicesMemo <> nil then
    DevicesMemo.Text := BuildInventoryCaption;

  if DriverStatusLabel = nil then
    Exit;

  DriverReady := InventoryNvidiaPresent and (InventoryNvidiaSmiOk or NvidiaDriverOk);

  if DriverRecheckBtn <> nil then
    DriverRecheckBtn.Visible := True;

  if not InventoryNvidiaPresent then
  begin
    DriverStatusLabel.Caption :=
      'No NVIDIA GPU detected.' + #13#10 +
      'You can continue. The agent can use the CPU for compatible models. ' +
      'A discrete NVIDIA GPU is required for GPU inference.';
    if DriverDownloadBtn <> nil then
      DriverDownloadBtn.Visible := False;
    Exit;
  end;

  if DriverReady then
  begin
    InstalledVer := '';
    if FileExists(InventoryIniPath) then
      InstalledVer := GetIniString('Inventory', 'NvidiaDriverVersion', '', InventoryIniPath);
    if InstalledVer <> '' then
      DriverStatusLabel.Caption :=
        'NVIDIA driver verified (version ' + InstalledVer + ').' + #13#10 +
        'This machine is ready for GPU inference after you connect a provider token.'
    else
      DriverStatusLabel.Caption :=
        'NVIDIA driver verified.' + #13#10 +
        'This machine is ready for GPU inference after you connect a provider token.';
    if DriverDownloadBtn <> nil then
      DriverDownloadBtn.Visible := False;
    Exit;
  end;

  { NVIDIA present but driver missing / broken — show download guidance }
  if DriverDownloadBtn <> nil then
    DriverDownloadBtn.Visible := True;

  if not DriverLookupDone then
    RunNvidiaDriverLookup;

  Err := '';
  if FileExists(DriverLookupIniPath) then
    Err := GetIniString('Driver', 'Error', '', DriverLookupIniPath);
  Laptop := '';
  if FileExists(DriverLookupIniPath) then
    if GetIniString('Driver', 'IsLaptop', '0', DriverLookupIniPath) = '1' then
      Laptop := ' (laptop package)';

  if (ResolvedDriverUrl <> '') and (ResolvedGpuName <> '') then
  begin
    DriverStatusLabel.Caption :=
      'NVIDIA GPU detected, but the driver is missing or not working.' + #13#10 +
      'Detected: ' + ResolvedGpuName + Laptop + #13#10 +
      'Install Game Ready or Studio driver ' + ResolvedDriverVersion +
      ', reboot if Windows asks, then click Recheck.' + #13#10 +
      'You may continue without it; the agent will use CPU-compatible models until a driver is installed.';
    if DriverDownloadBtn <> nil then
      DriverDownloadBtn.Caption := 'Download driver ' + ResolvedDriverVersion;
  end
  else if Err <> '' then
  begin
    DriverStatusLabel.Caption :=
      'NVIDIA GPU detected, but the driver is missing or not working.' + #13#10 +
      'Could not select a package automatically: ' + Err + #13#10 +
      'Download Game Ready or Studio drivers for your GPU from NVIDIA, install, ' +
      'reboot if asked, then click Recheck.';
    if DriverDownloadBtn <> nil then
      DriverDownloadBtn.Caption := 'Open NVIDIA driver download';
  end
  else
  begin
    DriverStatusLabel.Caption :=
      'NVIDIA GPU detected, but the driver is missing or not working.' + #13#10 +
      'Install a current Game Ready or Studio driver from NVIDIA, reboot if Windows asks, then click Recheck.' + #13#10 +
      'On laptops, use the OEM or NVIDIA laptop package for your exact model.';
    if DriverDownloadBtn <> nil then
      DriverDownloadBtn.Caption := 'Open NVIDIA driver download';
  end;
end;

procedure DriverDownloadClick(Sender: TObject);
var
  ErrorCode: Integer;
  Url: String;
begin
  if not DriverLookupDone then
    RunNvidiaDriverLookup;

  Url := ResolvedDriverUrl;
  if Url = '' then
    Url := 'https://www.nvidia.com/Download/index.aspx';

  ShellExec('open', Url, '', '', SW_SHOWNORMAL, ewNoWait, ErrorCode);
end;

procedure DriverRecheckClick(Sender: TObject);
begin
  DriverLookupDone := False;
  RefreshDevicesStatus;
  if InventoryNvidiaSmiOk or NvidiaDriverOk then
    MsgBox('NVIDIA driver verified. Click Next to continue.', mbInformation, MB_OK)
  else if not InventoryNvidiaPresent then
    MsgBox(
      'No NVIDIA GPU detected. You can continue; the agent can use the CPU for compatible models.',
      mbInformation, MB_OK)
  else if ResolvedDriverUrl <> '' then
    MsgBox(
      'The NVIDIA driver is still not available.' + #13#10 + #13#10 +
      'Download and install driver ' + ResolvedDriverVersion + ' for:' + #13#10 +
      '  ' + ResolvedGpuName + #13#10 + #13#10 +
      'Reboot if Windows asks, then click Recheck again.',
      mbError, MB_OK)
  else
    MsgBox(
      'The NVIDIA driver is still not available.' + #13#10 + #13#10 +
      'Install the driver, reboot if Windows asks, then click Recheck again.' + #13#10 +
      'On laptops, use the OEM or NVIDIA laptop package for your exact model.',
      mbError, MB_OK);
end;

function InitializeSetup(): Boolean;
begin
  PrefillToken := ExpandConstant('{param:TOKEN|}');
  IsSilentUpdate := ExpandConstant('{param:UPDATE|0}') = '1';
  LibDir := ExpandConstant('{localappdata}\Scalattice\lib');
  ModelsCacheDir := ExpandConstant('{userpf}\.cache\scalattice\models');
  ModelsCacheBytes := 0;
  ResolvedDriverUrl := '';
  ResolvedGpuName := '';
  ResolvedDriverVersion := '';
  DriverLookupDone := False;
  InventoryNvidiaPresent := False;
  InventoryNvidiaSmiOk := False;
  if DirExists(ModelsCacheDir) then
    GetDirSize(ModelsCacheDir, ModelsCacheBytes);
  { Silent updates skip interactive pages. }
  ShowModelsPage := (not WizardSilent) and (not IsSilentUpdate) and ShouldOfferModelPurge();
  { Always show Compatible devices (CPU + GPUs + driver status) on interactive installs. }
  ShowDevicesPage := (not WizardSilent) and (not IsSilentUpdate);
  Result := True;
end;

procedure InitializeWizard;
var
  SizeLabel: String;
  TokenAfterID: Integer;
begin
  TokenAfterID := wpWelcome;
  DevicesPage := nil;
  DevicesMemo := nil;
  DriverStatusLabel := nil;
  DriverDownloadBtn := nil;
  DriverRecheckBtn := nil;

  if ShowDevicesPage then
  begin
    DevicesPage := CreateCustomPage(
      wpWelcome,
      'Compatible devices',
      'Hardware & drivers Scalattice has detected on this PC.');
    TokenAfterID := DevicesPage.ID;

    DevicesMemo := TNewMemo.Create(DevicesPage);
    DevicesMemo.Parent := DevicesPage.Surface;
    DevicesMemo.Left := ScaleX(0);
    DevicesMemo.Top := ScaleY(0);
    DevicesMemo.Width := DevicesPage.SurfaceWidth;
    DevicesMemo.Height := ScaleY(118);
    DevicesMemo.ReadOnly := True;
    DevicesMemo.ScrollBars := ssVertical;
    DevicesMemo.WantReturns := True;
    DevicesMemo.Text := 'Scanning compute devices...';

    DriverStatusLabel := TNewStaticText.Create(DevicesPage);
    DriverStatusLabel.Parent := DevicesPage.Surface;
    DriverStatusLabel.Left := ScaleX(0);
    DriverStatusLabel.Top := ScaleY(126);
    DriverStatusLabel.Width := DevicesPage.SurfaceWidth;
    DriverStatusLabel.Height := ScaleY(78);
    DriverStatusLabel.WordWrap := True;
    DriverStatusLabel.AutoSize := False;
    DriverStatusLabel.Caption := 'Checking NVIDIA driver...';

    DriverDownloadBtn := TNewButton.Create(DevicesPage);
    DriverDownloadBtn.Parent := DevicesPage.Surface;
    DriverDownloadBtn.Left := ScaleX(0);
    DriverDownloadBtn.Top := ScaleY(212);
    DriverDownloadBtn.Width := ScaleX(220);
    DriverDownloadBtn.Height := ScaleY(28);
    DriverDownloadBtn.Caption := 'Download recommended driver';
    DriverDownloadBtn.OnClick := @DriverDownloadClick;
    DriverDownloadBtn.Visible := False;

    DriverRecheckBtn := TNewButton.Create(DevicesPage);
    DriverRecheckBtn.Parent := DevicesPage.Surface;
    DriverRecheckBtn.Left := ScaleX(232);
    DriverRecheckBtn.Top := ScaleY(212);
    DriverRecheckBtn.Width := ScaleX(100);
    DriverRecheckBtn.Height := ScaleY(28);
    DriverRecheckBtn.Caption := 'Recheck';
    DriverRecheckBtn.OnClick := @DriverRecheckClick;

    RefreshDevicesStatus;
  end;

  TokenPage := CreateInputQueryPage(
    TokenAfterID,
    'Connect to Scalattice',
    'Paste your provider machine token',
    'Create a machine in the Scalattice Providers dashboard and paste its token below.' + #13#10 +
    'The installer saves the token, adds Scalattice to your PATH, and starts the background agent.');
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
    PurgeModelsCheck.Left := ScaleX(0);
    PurgeModelsCheck.Top := ScaleY(8);
    PurgeModelsCheck.Width := ModelsPage.SurfaceWidth;
    PurgeModelsCheck.Caption := 'Remove stored models (' + SizeLabel + ')';
    PurgeModelsCheck.Checked := False;
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if (DevicesPage <> nil) and (CurPageID = DevicesPage.ID) then
    RefreshDevicesStatus;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (DevicesPage <> nil) and (CurPageID = DevicesPage.ID) then
  begin
    if InventoryNvidiaPresent and (not InventoryNvidiaSmiOk) and (not NvidiaDriverOk) then
    begin
      if MsgBox(
        'An NVIDIA GPU was detected, but no working driver is available.' + #13#10 + #13#10 +
        'GPU inference will be unavailable until a driver is installed. ' +
        'The agent can still use the CPU for compatible models.' + #13#10 + #13#10 +
        'Continue installing Scalattice Agent?',
        mbConfirmation, MB_YESNO) = IDNO then
        Result := False;
    end;
    Exit;
  end;
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
  if not CudaRuntimePresent(LibDir) then
  begin
    if not WizardSilent then
      MsgBox(
        'Install finished, but the CUDA 12 runtime was not copied to:' + #13#10 +
        '  ' + LibDir + #13#10#13#10 +
        'Required: cudart64_12.dll, cublas64_12.dll, cublasLt64_12.dll' + #13#10 +
        'Re-download the official Scalattice Agent installer and run setup again.' + #13#10 +
        'The agent was not started.',
        mbError, MB_OK);
    Exit;
  end;
  Exec('wscript.exe', '//nologo "' + AppDir + '\launch-background.vbs"',
    AppDir, SW_HIDE, ewNoWait, ResultCode);
  Sleep(800);
  Exec('wscript.exe', '//nologo "' + AppDir + '\launch-tray.vbs"',
    AppDir, SW_HIDE, ewNoWait, ResultCode);
end;

{ Always launch via scalattice-run.cmd so %LOCALAPPDATA%\Scalattice\lib is on PATH
  before Windows resolves cudart64_12.dll (bare .exe Exec causes the System Error dialog). }
procedure ExecAgent(const AppDir, Params: String; var ResultCode: Integer);
var
  RunCmd, CmdLine: String;
begin
  ResultCode := 1;
  if not CudaRuntimePresent(LibDir) then
    Exit;
  RunCmd := AppDir + '\scalattice-run.cmd';
  if FileExists(RunCmd) then
  begin
    CmdLine := '/c ""' + RunCmd + '" ' + Params + '"';
    Exec('cmd.exe', CmdLine, AppDir, SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end
  else
    Exec(AppDir + '\{#MyAppExeName}', Params, AppDir, SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

procedure PersistProviderToken(const Token: String);
var
  Dir, Path: String;
begin
  if not TokenLooksValid(Token) then
    Exit;
  Dir := ExpandConstant('{userpf}\.config\scalattice');
  if not DirExists(Dir) then
    ForceDirectories(Dir);
  Path := Dir + '\agent.env';
  SaveStringToFile(Path, 'SCALATTICE_AGENT_TOKEN=' + Token + #13#10, False);
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

  if (Token = '') and (SavedToken <> '') then
    Token := SavedToken;

  { Persist token even when we cannot launch the exe (missing CUDA libs). }
  if Token <> '' then
    PersistProviderToken(Token);

  if not CudaRuntimePresent(LibDir) then
  begin
    if not WizardSilent then
      MsgBox(
        'Files were installed, but the CUDA 12 runtime DLLs are missing from:' + #13#10 +
        '  ' + LibDir + #13#10#13#10 +
        'That causes the Windows "cudart64_12.dll was not found" error.' + #13#10 +
        'Re-download ScalatticeAgentSetup from the official Scalattice / GitHub release ' +
        'and run setup again.',
        mbError, MB_OK);
    Exit;
  end;

  if IsSilentUpdate or WizardSilent then
  begin
    { Prefer restart so same-token set-token cannot skip relaunching tray/background. }
    ExecAgent(AppDir, 'restart', ResultCode);
    if ResultCode <> 0 then
    begin
      if Token <> '' then
        ExecAgent(AppDir, 'set-token --token "' + Token + '"', ResultCode);
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
    ExecAgent(AppDir, 'set-token --token "' + Token + '"', ResultCode)
  else
    ResultCode := 1;

  LaunchScalatticeRuntime(AppDir);

  if Token = '' then
    MsgBox('Scalattice Agent was installed, but no provider token was found.' + #13#10 +
      'Open Command Prompt and run:' + #13#10 +
      '  scalattice-agent set-token --token YOUR_TOKEN',
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
