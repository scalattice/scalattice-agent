# Inspect a Windows release bundle / install layout for missing CUDA DLLs.
param(
    [string]$DistDir = "",
    [switch]$Installed
)

$ErrorActionPreference = "Stop"
$required = @("cudart64_12.dll", "cublas64_12.dll", "cublasLt64_12.dll")

if ($Installed) {
    $lib = Join-Path $env:LOCALAPPDATA "Scalattice\lib"
    $bin = Join-Path $env:LOCALAPPDATA "Scalattice\bin"
    Write-Host "==> Installed layout"
    Write-Host "    bin: $bin"
    Write-Host "    lib: $lib"
    foreach ($name in $required) {
        $libPath = Join-Path $lib $name
        $binPath = Join-Path $bin $name
        $ok = (Test-Path $libPath) -or (Test-Path $binPath)
        Write-Host ("    {0,-22} {1}" -f $name, ($(if ($ok) { "OK" } else { "MISSING" })))
    }
    if (Test-Path (Join-Path $bin "scalattice-run.cmd")) {
        Write-Host "    scalattice-run.cmd     OK"
    } else {
        Write-Host "    scalattice-run.cmd     MISSING"
    }
    foreach ($name in @("launch-tray.vbs", "launch-background.vbs")) {
        $path = Join-Path $bin $name
        Write-Host ("    {0,-22} {1}" -f $name, ($(if (Test-Path $path) { "OK" } else { "MISSING" })))
    }
    exit 0
}

if (-not $DistDir) {
    $DistDir = Join-Path (Split-Path $PSScriptRoot -Parent) "dist"
}

Write-Host "==> Checking dist bundle: $DistDir"
foreach ($name in $required) {
    $inLib = Test-Path (Join-Path $DistDir "lib\$name")
    $inRoot = Test-Path (Join-Path $DistDir $name)
    $status = if ($inLib -or $inRoot) { "OK" } else { "MISSING" }
    $where = @()
    if ($inLib) { $where += "lib" }
    if ($inRoot) { $where += "dist" }
    Write-Host ("    {0,-22} {1} ({2})" -f $name, $status, ($where -join ", "))
}

if (Test-Path (Join-Path $DistDir "scalattice-run.cmd")) {
    Write-Host "    scalattice-run.cmd     OK"
} else {
    Write-Host "    scalattice-run.cmd     MISSING"
}
foreach ($name in @("launch-tray.vbs", "launch-background.vbs")) {
    $path = Join-Path $DistDir $name
    Write-Host ("    {0,-22} {1}" -f $name, ($(if (Test-Path $path) { "OK" } else { "MISSING" })))
}

$setup = Join-Path $DistDir "ScalatticeAgentSetup-x86_64.exe"
if (Test-Path $setup) {
    $item = Get-Item $setup
    Write-Host ""
    Write-Host "==> Installer: $($item.FullName)"
    Write-Host "    size: $([math]::Round($item.Length / 1MB, 1)) MB"
    Write-Host "    modified: $($item.LastWriteTime)"
}
