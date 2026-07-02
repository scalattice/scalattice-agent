# Windows diagnostics: installed agent, release bundle, or CI build host.
param(
    [switch]$LaunchTray,
    [switch]$Bundle,
    [string]$DistDir = "",
    [switch]$InstalledOnly
)

$ErrorActionPreference = "Continue"

function Section($title) {
    Write-Host ""
    Write-Host "---- $title ----"
}

function Show-FileRow($label, $path) {
    $status = if (Test-Path -LiteralPath $path) { "OK" } else { "missing" }
    Write-Host ("  {0,-26} {1}" -f $label, $status)
}

function Show-CudaDlls($libDir, $binDir) {
    foreach ($name in @("cudart64_12.dll", "cublas64_12.dll", "cublasLt64_12.dll")) {
        $libPath = Join-Path $libDir $name
        $binPath = Join-Path $binDir $name
        $ok = (Test-Path -LiteralPath $libPath) -or (Test-Path -LiteralPath $binPath)
        Write-Host ("  {0,-26} {1}" -f $name, ($(if ($ok) { "OK" } else { "MISSING" })))
    }
}

function Test-BundleLayout {
    param(
        [Parameter(Mandatory)][string]$Root,
        [string]$Title
    )

    Section $Title
    Write-Host "  path: $Root"
    Show-CudaDlls (Join-Path $Root "lib") $Root
    foreach ($name in @("scalattice-run.cmd", "launch-tray.vbs", "launch-background.vbs", "scalattice-agent.exe")) {
        Show-FileRow $name (Join-Path $Root $name)
    }

    $setup = Join-Path $Root "ScalatticeAgentSetup-x86_64.exe"
    if (Test-Path -LiteralPath $setup) {
        $item = Get-Item -LiteralPath $setup
        Write-Host "  installer size: $([math]::Round($item.Length / 1MB, 1)) MB"
        Write-Host "  installer date: $($item.LastWriteTime)"
    }
}

if ($Bundle -or $InstalledOnly) {
    if ($InstalledOnly) {
        $bin = Join-Path $env:LOCALAPPDATA "Scalattice\bin"
        $lib = Join-Path $env:LOCALAPPDATA "Scalattice\lib"
        Test-BundleLayout -Root $bin -Title "Installed layout (bin)"
        Section "Installed CUDA libs"
        Show-CudaDlls $lib $bin
        exit 0
    }

    if (-not $DistDir) {
        $DistDir = Join-Path (Split-Path $PSScriptRoot -Parent) "dist"
    }
    Test-BundleLayout -Root $DistDir -Title "Release bundle (dist/)"
    exit 0
}

$bin = Join-Path $env:LOCALAPPDATA "Scalattice\bin"
$lib = Join-Path $env:LOCALAPPDATA "Scalattice\lib"
$logs = Join-Path $env:LOCALAPPDATA "Scalattice\logs"
$exe = Join-Path $bin "scalattice-agent.exe"
$run = Join-Path $bin "scalattice-run.cmd"

Write-Host ""
Write-Host "========== Scalattice Agent diagnostics =========="

Section "Install version"
if (Test-Path -LiteralPath $exe) {
    $vi = (Get-Item -LiteralPath $exe).VersionInfo
    Write-Host "  File version : $($vi.FileVersion)"
    Write-Host "  Product      : $($vi.ProductVersion)"
} else {
    Write-Host "  MISSING $exe"
}

Section "Installed files"
foreach ($name in @(
    "scalattice-agent.exe", "scalattice-run.cmd", "launch-tray.vbs",
    "launch-background.vbs", "open-tray-debug.cmd", "run-background.cmd", "tray.pid"
)) {
    Show-FileRow $name (Join-Path $bin $name)
}

Section "CUDA / bundled DLLs"
Show-CudaDlls $lib $bin

Section "Token config"
$envFile = Join-Path $env:USERPROFILE ".config\scalattice\agent.env"
if (Test-Path -LiteralPath $envFile) {
    $hasToken = Select-String -Path $envFile -Pattern '^SCALATTICE_AGENT_TOKEN=' -Quiet
    Write-Host ("  agent.env                {0}" -f ($(if ($hasToken) { "token set" } else { "token missing" })))
} else {
    Write-Host "  agent.env                missing"
}

Section "Processes"
$procs = Get-Process scalattice-agent -ErrorAction SilentlyContinue
if ($procs) {
    $procs | Format-Table Id, StartTime -AutoSize | Out-String | Write-Host
} else {
    Write-Host "  (no scalattice-agent.exe running)"
}

Section "CLI status"
if (Test-Path -LiteralPath $run) {
    & $run status 2>&1
} else {
    Write-Host "  scalattice-run.cmd missing"
}

Section "Agent log (last 15 lines)"
$agentLog = Join-Path $logs "agent.log"
if (Test-Path -LiteralPath $agentLog) {
    Get-Content -LiteralPath $agentLog -Tail 15 | ForEach-Object { Write-Host "  $_" }
} else {
    Write-Host "  (no agent.log)"
}

Section "Tray log (last 20 lines)"
$trayLog = Join-Path $logs "tray.log"
if (Test-Path -LiteralPath $trayLog) {
    Get-Content -LiteralPath $trayLog -Tail 20 | ForEach-Object { Write-Host "  $_" }
} else {
    Write-Host "  (no tray.log)"
}

Section "Autostart"
$startup = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\Startup"
Get-ChildItem -LiteralPath $startup -Filter "Scalattice*" -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "  Startup\$($_.Name)"
}
foreach ($tn in @("ScalatticeAgent", "ScalatticeAgentTray")) {
    schtasks /Query /TN $tn 2>$null | Out-Null
    Write-Host ("  schtasks {0,-18} {1}" -f $tn, ($(if ($LASTEXITCODE -eq 0) { "registered" } else { "missing" })))
}

Section "Start Menu shortcuts"
$sm = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs"
Get-ChildItem -LiteralPath $sm -Recurse -Filter "*Scalattice*" -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "  $($_.FullName)"
}

Write-Host ""
Write-Host "========== Manual tests =========="
Write-Host "  cd `"$bin`""
Write-Host "  .\open-tray-debug.cmd"
Write-Host "  .\scalattice-agent.exe tray --force"
Write-Host ""
Write-Host "Bundle check:  .\scripts\diagnose-windows.ps1 -Bundle"
Write-Host "Installed DLLs:  .\scripts\diagnose-windows.ps1 -InstalledOnly"
Write-Host ""

if ($LaunchTray) {
    Section "Launching tray (debug)"
    if (-not (Test-Path -LiteralPath $exe)) { Write-Error "exe missing" }
    $env:PATH = "$bin;$lib;$env:PATH"
    Remove-Item Env:SCALATTICE_TRAY_HIDDEN -ErrorAction SilentlyContinue
    Push-Location $bin
    try {
        & $exe tray --force
    } finally {
        Pop-Location
    }
}
