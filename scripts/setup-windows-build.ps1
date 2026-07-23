# One-time Windows build host setup (Visual Studio C++, CUDA 12.6, Inno Setup, Rust).
# Run as Administrator.
#
# Usage (Admin cmd):
#   scripts\setup-windows-build.cmd

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
. (Join-Path $PSScriptRoot "windows-build-common.ps1")

if (-not (Test-Admin)) {
    Write-Error "Run this script in an elevated (Administrator) PowerShell."
}

Ensure-PowerShellExecutionPolicy

Ensure-ShortBuildDirs

Ensure-Chocolatey

Write-Host "==> Installing build dependencies (skips already-installed packages)"

if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Invoke-Choco @("install", "-y", "--no-progress", "git")
} else {
    Write-Host "==> git already installed"
}

if (-not (Test-Path "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe") -and
    -not (Test-Path "${env:ProgramFiles}\Inno Setup 6\ISCC.exe")) {
    Invoke-Choco @("install", "-y", "--no-progress", "innosetup")
} else {
    Write-Host "==> Inno Setup already installed"
}

Install-SystemWideRust

Ensure-BuildMachinePath

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$hasVcTools = $false
if (Test-Path $vswhere) {
    $installPath = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath 2>$null
    $hasVcTools = [bool]$installPath
}
if (-not $hasVcTools) {
    Write-Host "==> Installing Visual Studio 2022 Build Tools (C++)"
    $vsParams = '--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended'
    Invoke-Choco @("install", "-y", "--no-progress", "visualstudio2022buildtools", "--package-parameters", $vsParams)
} else {
    Write-Host "==> Visual Studio C++ tools already installed"
}

Install-CudaToolkit

# Rust may land on PATH only in new shells; refresh common locations.
$rustBins = @(
    (Get-SystemRustCargoBin),
    "$env:USERPROFILE\.cargo\bin"
)
foreach ($rustBin in $rustBins) {
    if ((Test-Path $rustBin) -and -not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        $env:PATH = "$rustBin;$env:PATH"
    }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo not on PATH - open a new Administrator PowerShell and re-run this script"
}

$nvcc = Find-Nvcc
if (-not $nvcc) {
    Write-Error "CUDA nvcc still not found"
}

Write-Host ""
Write-Host "==> Windows build host ready."
Write-Host "    CUDA:  $nvcc"
Write-Host "    Also install Vulkan SDK (build host only): https://vulkan.lunarg.com/"
Write-Host "    Core components only — providers do not need the SDK."
Write-Host "    Next:  scripts\install-windows-runner.cmd"
