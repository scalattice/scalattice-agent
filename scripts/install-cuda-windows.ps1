# Install NVIDIA CUDA Toolkit 12.6 only (Administrator).
# Run if setup-windows-build.cmd failed on the cuda step.
#
#   scripts\install-cuda-windows.cmd

$ErrorActionPreference = "Stop"

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

if (-not (Test-Admin)) {
    Write-Error "Run as Administrator."
}

$existing = Find-Nvcc
if ($existing) {
    Write-Host "==> CUDA already installed: $existing"
    exit 0
}

if (-not (Get-Command choco -ErrorAction SilentlyContinue)) {
    Write-Error "Chocolatey required. Run scripts\setup-windows-build.cmd first."
}

Write-Host "==> Installing CUDA 12.6.3 via Chocolatey (10-20 min)"
$cudaVersion = "12.6.3.561"
try {
    Invoke-Choco @("install", "-y", "--no-progress", "cuda", "--version=$cudaVersion")
} catch {
    Write-Warning "cuda $cudaVersion failed, trying latest cuda package..."
    Invoke-Choco @("install", "-y", "--no-progress", "cuda")
}

$nvcc = Find-Nvcc
if (-not $nvcc) {
    Write-Error "CUDA still not found. Try: choco install -y cuda --version=12.6.3.561"
}

Write-Host "==> CUDA OK: $nvcc"
Write-Host "==> Next: scripts\install-windows-runner.cmd"
