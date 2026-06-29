# Shared helpers for Windows setup scripts (dot-sourced).

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $p = New-Object Security.Principal.WindowsPrincipal($id)
    return $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Ensure-Chocolatey {
    if (Get-Command choco -ErrorAction SilentlyContinue) {
        return
    }

    $chocoRoot = "${env:ProgramData}\chocolatey"
    $chocoBin = Join-Path $chocoRoot "bin"
    $chocoExe = Join-Path $chocoBin "choco.exe"

    if (Test-Path $chocoExe) {
        Write-Host "==> Chocolatey found at $chocoBin (adding to PATH for this session)"
        $env:PATH = "$chocoBin;$env:PATH"
        if (Get-Command choco -ErrorAction SilentlyContinue) {
            return
        }
    }

    Write-Host "==> Installing Chocolatey"
    Set-ExecutionPolicy Bypass -Scope Process -Force
    Invoke-Expression ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))

    $machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $env:PATH = "$machinePath;$userPath"
    if ((Test-Path $chocoExe) -and -not (Get-Command choco -ErrorAction SilentlyContinue)) {
        $env:PATH = "$chocoBin;$env:PATH"
    }

    if (-not (Get-Command choco -ErrorAction SilentlyContinue)) {
        Write-Error "Chocolatey is not available. Open a new Administrator terminal or reinstall from https://chocolatey.org/install"
    }
}

function Invoke-Choco {
    param([Parameter(Mandatory = $true)][string[]]$InstallArgs)
    Ensure-Chocolatey
    & choco @InstallArgs
    if ($LASTEXITCODE -ne 0) {
        throw "choco failed (exit $LASTEXITCODE): choco $($InstallArgs -join ' ')"
    }
}

function Add-MachinePathEntry {
    param([string]$Dir)

    if (-not $Dir -or -not (Test-Path $Dir)) { return }
    $machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $parts = @($machinePath -split ';' | Where-Object { $_ })
    if ($parts -contains $Dir) { return }
    [Environment]::SetEnvironmentVariable("Path", "$Dir;$machinePath", "Machine")
    Write-Host "==> Added to Machine PATH: $Dir"
}

function Install-SystemWideRust {
    $rustRoot = "C:\Rust"
    $cargoHome = Join-Path $rustRoot "cargo"
    $rustupHome = Join-Path $rustRoot "rustup"
    $cargoExe = Join-Path $cargoHome "bin\cargo.exe"

    if (Test-Path $cargoExe) {
        Write-Host "==> System-wide Rust: $cargoExe"
        return $cargoHome
    }

    Write-Host "==> Installing system-wide Rust at $rustRoot (for GHA runner service account)"
    New-Item -ItemType Directory -Force -Path (Join-Path $cargoHome "bin"), $rustupHome | Out-Null

    $env:RUSTUP_HOME = $rustupHome
    $env:CARGO_HOME = $cargoHome
    [Environment]::SetEnvironmentVariable("RUSTUP_HOME", $rustupHome, "Machine")
    [Environment]::SetEnvironmentVariable("CARGO_HOME", $cargoHome, "Machine")

    $rustupInit = Join-Path $env:TEMP "rustup-init.exe"
    Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile $rustupInit
    & $rustupInit -y --default-toolchain stable --no-modify-path
    if ($LASTEXITCODE -ne 0) {
        throw "rustup-init failed (exit $LASTEXITCODE)"
    }

    return $cargoHome
}

function Get-WindowsBuildPathEntries {
    $paths = @()

    foreach ($dir in @(
            "${env:ProgramData}\chocolatey\bin",
            "${env:ProgramFiles}\Git\bin",
            "${env:ProgramFiles}\Git\usr\bin",
            "C:\Rust\cargo\bin",
            "$env:USERPROFILE\.cargo\bin"
        )) {
        if ((Test-Path $dir) -and ($paths -notcontains $dir)) {
            $paths += $dir
        }
    }

    $cuda = $env:CUDA_PATH
    if (-not $cuda) {
        $cuda = "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6"
    }
    if ($cuda -and (Test-Path "$cuda\bin")) {
        $paths += "$cuda\bin"
    }

    return $paths
}

function Ensure-BuildMachinePath {
    Add-MachinePathEntry "${env:ProgramData}\chocolatey\bin"
    Add-MachinePathEntry "${env:ProgramFiles}\Git\bin"
    Add-MachinePathEntry "${env:ProgramFiles}\Git\usr\bin"

    $cargoHome = Install-SystemWideRust
    if ($cargoHome) {
        Add-MachinePathEntry (Join-Path $cargoHome "bin")
    }

    $cuda = "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6"
    if (Test-Path "$cuda\bin") {
        Add-MachinePathEntry "$cuda\bin"
        if (-not $env:CUDA_PATH) {
            [Environment]::SetEnvironmentVariable("CUDA_PATH", $cuda, "Machine")
        }
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

Try manually:
  choco install -y cuda --version=12.6.3.561

Or download:
  https://developer.nvidia.com/cuda-12-6-3-download-archive
"@
    }
    Write-Host "==> CUDA OK: $nvcc"
}
