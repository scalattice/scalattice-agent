# One-shot Scalattice Agent diagnostics for Windows. Copy to the machine or run from the repo.
param(
    [switch]$LaunchTray
)

$bin = Join-Path $env:LOCALAPPDATA "Scalattice\bin"
$lib = Join-Path $env:LOCALAPPDATA "Scalattice\lib"
$logs = Join-Path $env:LOCALAPPDATA "Scalattice\logs"
$exe = Join-Path $bin "scalattice-agent.exe"
$run = Join-Path $bin "scalattice-run.cmd"

Write-Host ""
Write-Host "========== Scalattice Agent diagnostics =========="
Write-Host ""

function Section($title) {
    Write-Host "---- $title ----"
}

Section "Install version"
if (Test-Path $exe) {
    $vi = (Get-Item $exe).VersionInfo
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
    $p = Join-Path $bin $name
    Write-Host ("  {0,-24} {1}" -f $name, ($(if (Test-Path $p) { "OK" } else { "missing" })))
}

Section "Processes"
$procs = Get-Process scalattice-agent -ErrorAction SilentlyContinue
if ($procs) {
    $procs | Format-Table Id, StartTime -AutoSize | Out-String | Write-Host
} else {
    Write-Host "  (no scalattice-agent.exe running)"
}

Section "CLI status"
if (Test-Path $run) {
    & $run status 2>&1
} else {
    Write-Host "  scalattice-run.cmd missing"
}

Section "Agent log (last 15 lines)"
$agentLog = Join-Path $logs "agent.log"
if (Test-Path $agentLog) {
    Get-Content $agentLog -Tail 15 | ForEach-Object { Write-Host "  $_" }
} else {
    Write-Host "  (no agent.log)"
}

Section "Tray log (last 20 lines)"
$trayLog = Join-Path $logs "tray.log"
if (Test-Path $trayLog) {
    Get-Content $trayLog -Tail 20 | ForEach-Object { Write-Host "  $_" }
} else {
    Write-Host "  (no tray.log — tray has never started successfully)"
}

Section "Autostart"
$startup = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\Startup"
Get-ChildItem $startup -Filter "Scalattice*" -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "  Startup\$($_.Name)"
}
foreach ($tn in @("ScalatticeAgent", "ScalatticeAgentTray")) {
    schtasks /Query /TN $tn 2>$null | Out-Null
    Write-Host ("  schtasks {0,-20} {1}" -f $tn, ($(if ($LASTEXITCODE -eq 0) { "registered" } else { "missing" })))
}

Section "Start Menu shortcut"
$sm = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs"
Get-ChildItem $sm -Recurse -Filter "*Scalattice*" -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "  $($_.FullName)"
}

Write-Host ""
Write-Host "========== Manual tests (run these next) =========="
Write-Host ""
Write-Host "  cd `"$bin`""
Write-Host "  .\open-tray-debug.cmd          # should open GUI + keep console open"
Write-Host "  .\scalattice-agent.exe tray --force"
Write-Host "  Get-Content `"$trayLog`" -Wait   # watch tray log while launching"
Write-Host ""

if ($LaunchTray) {
    Section "Launching tray (debug)"
    if (-not (Test-Path $exe)) { Write-Error "exe missing" }
    $env:PATH = "$bin;$lib;$env:PATH"
    Remove-Item Env:SCALATTICE_TRAY_HIDDEN -ErrorAction SilentlyContinue
    Push-Location $bin
    try {
        & $exe tray --force
    } finally {
        Pop-Location
    }
}
