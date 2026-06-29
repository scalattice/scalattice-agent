# One-time Windows build host setup (Visual Studio C++, CUDA 12.6, Inno Setup, Rust).
# Run as Administrator.
#
# Usage (Admin PowerShell or cmd):
#   scripts\setup-windows-build.cmd
#
# CUDA only (if the main script failed on cuda):
#   scripts\install-cuda-windows.cmd

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $p = New-Object Security.Principal.WindowsPrincipal($id)
    return $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Invoke-Choco {
    param([Parameter(Mandatory = $true)][string[]]$InstallArgs)
    & choco @InstallArgs
    if ($LASTEXITCODE -ne 0) {
        throw "choco failed (exit $LASTEXITCODE): choco $($InstallArgs -join ' ')"
    }
}

function Find-Nvcc {
    $candidates = @(
        $env:CUDA_PATH,
        "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6",
        "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6.3"
    ) | Where-Object { $_ -and (Test-Path $_) }

    foreach ($root in $candidates) {
        $nvcc = Join-Path $root "bin\nvcc.exe"
        if (Test-Path $nvcc) { return $nvcc }
    }

    $toolkitRoot = "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA"
    if (Test-Path $toolkitRoot) {
        $nvcc = Get-ChildItem -Path $toolkitRoot -Recurse -Filter nvcc.exe -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($nvcc) { return $nvcc.FullName }
    }
    return $null
}

function Install-CudaToolkit {
    $existing = Find-Nvcc
    if ($existing) {
        Write-Host "==> CUDA already installed: $existing"
        return
    }

    Write-Host "==> Installing NVIDIA CUDA Toolkit 12.6.3 (large download, 10-20 min)"
    # Chocolatey version must be the full package id (not 12.6.3 alone).
    $cudaVersion = "12.6.3.561"
    try {
        Invoke-Choco @("install", "-y", "--no-progress", "cuda", "--version=$cudaVersion")
    } catch {
        Write-Warning "Chocolatey cuda $cudaVersion failed: $_"
        Write-Host "==> Trying latest cuda package from Chocolatey..."
        Invoke-Choco @("install", "-y", "--no-progress", "cuda")
    }

    $nvcc = Find-Nvcc
    if (-not $nvcc) {
        Write-Error @"
CUDA toolkit not found after install.

Install manually, then re-run this script (it will skip installed components):
  choco install -y cuda --version=12.6.3.561

Or download from:
  https://developer.nvidia.com/cuda-12-6-3-download-archive
"@
    }
    Write-Host "==> CUDA OK: $nvcc"
}

if (-not (Test-Admin)) {
    Write-Error "Run this script in an elevated (Administrator) PowerShell."
}

if (-not (Get-Command choco -ErrorAction SilentlyContinue)) {
    Write-Host "==> Installing Chocolatey"
    Set-ExecutionPolicy Bypass -Scope Process -Force
    Invoke-Expression ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))
}

Write-Host "==> Installing build dependencies (may take 30-60 min first time)"

if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Invoke-Choco @("install", "-y", "--no-progress", "git")
}
if (-not (Test-Path "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe") -and
    -not (Test-Path "${env:ProgramFiles}\Inno Setup 6\ISCC.exe")) {
    Invoke-Choco @("install", "-y", "--no-progress", "innosetup")
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Invoke-Choco @("install", "-y", "--no-progress", "rust")
}

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
Write-Host "    Next:  scripts\install-windows-runner.cmd"
