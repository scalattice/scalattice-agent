# One-time Windows build host setup (Visual Studio C++, CUDA 12.6, Inno Setup, Rust).
# Run as Administrator.
#
# Usage (Admin PowerShell or cmd):
#   scripts\setup-windows-build.cmd
#
# Or in PowerShell:
#   Set-ExecutionPolicy Bypass -Scope Process -Force
#   .\scripts\setup-windows-build.ps1

$ErrorActionPreference = "Stop"

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $p = New-Object Security.Principal.WindowsPrincipal($id)
    return $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

if (-not (Test-Admin)) {
    Write-Error "Run this script in an elevated (Administrator) PowerShell."
}

if (-not (Get-Command choco -ErrorAction SilentlyContinue)) {
    Write-Host "==> Installing Chocolatey"
    Set-ExecutionPolicy Bypass -Scope Process -Force
    Invoke-Expression ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))
}

Write-Host "==> Installing build dependencies (may take 30–60 min first time)"
choco install -y --no-progress git innosetup
choco install -y --no-progress rust
choco install -y --no-progress visualstudio2022buildtools --package-parameters "--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
choco install -y --no-progress cuda --version=12.6.3

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (Test-Path $vswhere) {
    $installPath = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath
    if (-not $installPath) {
        Write-Warning "VC++ tools workload missing — install 'Desktop development with C++' in Visual Studio Installer"
    }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo not on PATH after rust install — open a new PowerShell and re-run"
}

Write-Host ""
Write-Host "==> Windows build host ready. Next:"
Write-Host "    .\scripts\install-windows-runner.ps1"
