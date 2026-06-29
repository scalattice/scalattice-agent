# Install NVIDIA CUDA Toolkit 12.6 only (Administrator).
#   scripts\install-cuda-windows.cmd

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "windows-build-common.ps1")

if (-not (Test-Admin)) {
    Write-Error "Run as Administrator."
}

Ensure-Chocolatey
Install-CudaToolkit

Write-Host "==> Next: scripts\install-windows-runner.cmd"
