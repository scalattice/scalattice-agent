# Shared helpers for Windows setup scripts (dot-sourced).

function Ensure-PowerShellExecutionPolicy {
    if (-not (Test-Admin)) { return }

    try {
        $machinePolicy = Get-ExecutionPolicy -Scope LocalMachine
        if ($machinePolicy -eq 'Restricted' -or $machinePolicy -eq 'Undefined') {
            Write-Host "==> Setting PowerShell execution policy to RemoteSigned (LocalMachine)"
            Set-ExecutionPolicy RemoteSigned -Scope LocalMachine -Force -ErrorAction Stop
            Write-Host "==> LocalMachine execution policy set to RemoteSigned"
        } else {
            Write-Host "==> LocalMachine execution policy already: $machinePolicy"
        }
    } catch {
        Write-Warning @"
Could not change LocalMachine execution policy: $_

This is usually fine. Self-hosted CI already runs scripts with -ExecutionPolicy Bypass.
If a future GHA step fails with 'running scripts is disabled', set policy in an
elevated PowerShell (not cmd):

  powershell -Command "Set-ExecutionPolicy RemoteSigned -Scope LocalMachine -Force"

Or check overrides with:

  powershell -Command "Get-ExecutionPolicy -List"
"@
    }
}

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

function Get-SystemRustCargoBin {
    return "C:\Rust\cargo\bin"
}

function Install-SystemWideRust {
    $rustRoot = "C:\Rust"
    $cargoHome = Join-Path $rustRoot "cargo"
    $rustupHome = Join-Path $rustRoot "rustup"
    $cargoExe = Join-Path $cargoHome "bin\cargo.exe"

    if (Test-Path $cargoExe) {
        Write-Host "==> System-wide Rust: $cargoExe"
        return
    }

    Write-Host "==> Installing system-wide Rust at $rustRoot (for GHA runner service account)"
    New-Item -ItemType Directory -Force -Path (Join-Path $cargoHome "bin"), $rustupHome | Out-Null

    $env:RUSTUP_HOME = $rustupHome
    $env:CARGO_HOME = $cargoHome
    $env:RUSTUP_INIT_SKIP_PATH_CHECK = "yes"
    [Environment]::SetEnvironmentVariable("RUSTUP_HOME", $rustupHome, "Machine")
    [Environment]::SetEnvironmentVariable("CARGO_HOME", $cargoHome, "Machine")

    $rustupInit = Join-Path $env:TEMP "rustup-init.exe"
    Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile $rustupInit
    & $rustupInit -y --default-toolchain stable --no-modify-path *> $null
    if ($LASTEXITCODE -ne 0) {
        throw "rustup-init failed (exit $LASTEXITCODE)"
    }
    if (-not (Test-Path $cargoExe)) {
        throw "rustup-init completed but cargo.exe missing at $cargoExe"
    }
    Write-Host "==> System-wide Rust installed: $cargoExe"
}

function Test-LibClangCandidate {
    param([string]$Dir)

    if ([string]::IsNullOrWhiteSpace($Dir)) { return $false }
    if ($Dir -notmatch '^[a-zA-Z]:\\') { return $false }
    return Test-Path -LiteralPath (Join-Path $Dir "libclang.dll")
}

function Repair-LibClangMachineEnv {
    $stored = [Environment]::GetEnvironmentVariable("LIBCLANG_PATH", "Machine")
    if ($stored -and -not (Test-LibClangCandidate $stored)) {
        Write-Warning "Removing invalid machine LIBCLANG_PATH"
        [Environment]::SetEnvironmentVariable("LIBCLANG_PATH", $null, "Machine")
    }
    if ($env:LIBCLANG_PATH -and -not (Test-LibClangCandidate $env:LIBCLANG_PATH)) {
        Remove-Item Env:\LIBCLANG_PATH -ErrorAction SilentlyContinue
    }
}

function Find-LibClangDir {
    Repair-LibClangMachineEnv

    $candidates = @(
        $env:LIBCLANG_PATH,
        "${env:ProgramFiles}\LLVM\bin"
    )

    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $vsRoot = & $vswhere -latest -products * -property installationPath 2>$null
        if ($vsRoot) {
            $candidates += @(
                (Join-Path $vsRoot "VC\Tools\Llvm\bin"),
                (Join-Path $vsRoot "VC\Tools\Llvm\x64\bin")
            )
        }
    }

    foreach ($dir in $candidates) {
        if (Test-LibClangCandidate $dir) {
            return $dir
        }
    }
    return $null
}

function Install-LibClang {
    $existing = Find-LibClangDir
    if ($existing) {
        Write-Host "==> libclang: $existing"
        return $existing
    }

    Write-Host "==> Installing LLVM (libclang.dll for bindgen)"
    Ensure-Chocolatey
    & choco install -y --no-progress llvm *> $null
    if ($LASTEXITCODE -ne 0) {
        throw "choco install llvm failed (exit $LASTEXITCODE)"
    }

    $dir = Find-LibClangDir
    if (-not $dir) {
        Write-Error "llvm package installed but libclang.dll not found"
    }
    Write-Host "==> libclang installed: $dir"
    return $dir
}

function Set-LibClangEnv {
    param([string]$Dir)

    if (-not (Test-LibClangCandidate $Dir)) {
        Write-Warning "Skipping invalid LIBCLANG_PATH: $Dir"
        return
    }
    $env:LIBCLANG_PATH = $Dir
    [Environment]::SetEnvironmentVariable("LIBCLANG_PATH", $Dir, "Machine")
    Add-MachinePathEntry $Dir
}

function Get-WindowsBuildPathEntries {
    $paths = @()

    foreach ($dir in @(
            "${env:ProgramData}\chocolatey\bin",
            "${env:ProgramFiles}\Git\bin",
            (Get-SystemRustCargoBin),
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

    $clang = Find-LibClangDir
    if ($clang -and ($paths -notcontains $clang)) {
        $paths += $clang
    }

    return $paths
}

function Ensure-BuildMachinePath {
    Add-MachinePathEntry "${env:ProgramData}\chocolatey\bin"
    Add-MachinePathEntry "${env:ProgramFiles}\Git\bin"

    Install-SystemWideRust
    Add-MachinePathEntry (Get-SystemRustCargoBin)

    $cuda = "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6"
    if (Test-Path "$cuda\bin") {
        Add-MachinePathEntry "$cuda\bin"
        if (-not $env:CUDA_PATH) {
            [Environment]::SetEnvironmentVariable("CUDA_PATH", $cuda, "Machine")
        }
    }

    Set-LibClangEnv -Dir (Install-LibClang)
}

function Remove-GitUsrBinFromPath {
    $block = @(
        (Join-Path ${env:ProgramFiles} "Git\usr\bin"),
        (Join-Path ${env:ProgramFiles(x86)} "Git\usr\bin")
    )
    $parts = @($env:PATH -split ';' | Where-Object { $_ -and ($block -notcontains $_) })
    $env:PATH = ($parts -join ';')
}

function Test-MsvcLinkerOnPath {
    $link = Get-Command link.exe -ErrorAction SilentlyContinue
    if (-not $link) { return $false }
    return $link.Source -notmatch '\\Git\\usr\\bin\\'
}

function Import-VsDevEnvironment {
    Remove-GitUsrBinFromPath

    if ((Get-Command cl.exe -ErrorAction SilentlyContinue) -and (Test-MsvcLinkerOnPath)) {
        Write-Host "==> MSVC toolchain on PATH"
        Write-Host "    cl:   $((Get-Command cl.exe).Source)"
        Write-Host "    link: $((Get-Command link.exe).Source)"
        return
    }

    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) {
        Write-Error "vswhere not found - install Visual Studio 2022 Build Tools with C++ workload"
    }

    $installPath = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath
    if (-not $installPath) {
        Write-Error "Visual Studio C++ tools not found - run scripts\setup-windows-build.cmd"
    }

    $devCmd = Join-Path $installPath "Common7\Tools\VsDevCmd.bat"
    if (-not (Test-Path $devCmd)) {
        Write-Error "VsDevCmd.bat not found under $installPath"
    }

    Write-Host "==> Loading MSVC environment from VsDevCmd.bat"
    $envDump = cmd.exe /c "`"$devCmd`" -no_logo -arch=amd64 -host_arch=amd64 && set"
    foreach ($line in $envDump) {
        $eq = $line.IndexOf('=')
        if ($eq -lt 1) { continue }
        $name = $line.Substring(0, $eq)
        $value = $line.Substring($eq + 1)
        [Environment]::SetEnvironmentVariable($name, $value, 'Process')
    }

    if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
        Write-Error "MSVC cl.exe not found after VsDevCmd.bat"
    }
    if (-not (Test-MsvcLinkerOnPath)) {
        Write-Error "MSVC link.exe not found after VsDevCmd.bat (check PATH for Git usr\bin conflicts)"
    }
    Write-Host "==> MSVC cl:   $((Get-Command cl.exe).Source)"
    Write-Host "==> MSVC link: $((Get-Command link.exe).Source)"
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
