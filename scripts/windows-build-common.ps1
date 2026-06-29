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

function Get-SystemRustupHome {
    return "C:\Rust\rustup"
}

function Get-SystemCargoHome {
    return "C:\Rust\cargo"
}

function Set-SystemRustEnv {
    param([switch]$ExportForCi)

    $cargoHome = Get-SystemCargoHome
    $rustupHome = Get-SystemRustupHome
    $cargoExe = Join-Path $cargoHome "bin\cargo.exe"

    if (Test-Path $cargoExe) {
        $env:CARGO_HOME = $cargoHome
        $env:RUSTUP_HOME = $rustupHome
    } elseif ($env:CARGO_HOME) {
        if (-not $env:RUSTUP_HOME) {
            $env:RUSTUP_HOME = Join-Path (Split-Path $env:CARGO_HOME -Parent) "rustup"
        }
    } else {
        return $false
    }

    if ($ExportForCi -and $env:GITHUB_ENV) {
        "CARGO_HOME=$($env:CARGO_HOME)" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
        "RUSTUP_HOME=$($env:RUSTUP_HOME)" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    }

    return $true
}

function Get-ChocolateyRustPathEntries {
    return @(
        (Join-Path ${env:ProgramData} "chocolatey\lib\rust\tools"),
        (Join-Path ${env:ProgramData} "chocolatey\lib\rust\tools\bin")
    )
}

function Remove-ChocolateyRustFromPath {
    $block = Get-ChocolateyRustPathEntries
    $parts = @($env:PATH -split ';' | Where-Object { $_ -and ($block -notcontains $_) })
    $env:PATH = ($parts -join ';')
}

function Invoke-IcaclsGrant {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Grant,
        [switch]$Recurse
    )

    if (-not (Test-Path -LiteralPath $Path)) { return }

    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $args = @(
            $Path,
            '/grant',
            $Grant,
            '/C',
            '/Q'
        )
        if ($Recurse) { $args += '/T' }
        & icacls @args 1>$null 2>$null
    } finally {
        $ErrorActionPreference = $prev
    }
}

function Ensure-RustToolchainPermissions {
    if (-not (Test-Admin)) { return }

    $root = "C:\Rust"
    New-Item -ItemType Directory -Force -Path $root, (Get-SystemCargoHome), (Get-SystemRustupHome), (Get-SystemRustCargoBin) | Out-Null

    foreach ($dir in @($root, (Get-SystemCargoHome), (Get-SystemRustupHome), (Get-SystemRustCargoBin))) {
        # (OI)(CI) inherits to new files/dirs without walking broken rustup proxy links.
        Invoke-IcaclsGrant $dir "NT AUTHORITY\NETWORK SERVICE:(OI)(CI)M"
        Invoke-IcaclsGrant $dir "BUILTIN\Administrators:(OI)(CI)F"
    }
    Write-Host "==> Rust toolchain permissions set for runner service"
}

function Test-SystemRustExecutable {
    param([string]$Exe)

    if (-not (Test-Path -LiteralPath $Exe)) { return $false }
    try {
        $null = & $Exe --version 2>&1
        return $LASTEXITCODE -eq 0
    } catch {
        return $false
    }
}

function Repair-SystemRustToolchain {
    $rustBin = Get-SystemRustCargoBin
    $rustup = Join-Path $rustBin "rustup.exe"
    $rustc = Join-Path $rustBin "rustc.exe"

    if (-not (Test-Path -LiteralPath $rustup)) {
        return $false
    }

    if (Test-SystemRustExecutable $rustc) {
        return $true
    }

    Write-Host "==> Repairing system Rust toolchain (rustc missing or broken)"
    & $rustup self repair 2>&1 | Out-Host
    & $rustup toolchain install stable --profile minimal -y 2>&1 | Out-Host
    if ($LASTEXITCODE -ne 0) { return $false }

    & $rustup default stable 2>&1 | Out-Host
    return (Test-SystemRustExecutable $rustc)
}

function Assert-SystemRustToolchain {
    param([switch]$ExportForCi)

    if (-not (Prioritize-SystemRustOnPath -ExportForCi:$ExportForCi)) {
        throw "System Rust not configured at C:\Rust\cargo\bin"
    }

    $rustBin = Get-SystemRustCargoBin
    $rustc = Join-Path $rustBin "rustc.exe"
    $cargo = Join-Path $rustBin "cargo.exe"
    $rustup = Join-Path $rustBin "rustup.exe"

    if (-not (Test-SystemRustExecutable $rustc)) {
        if (-not (Repair-SystemRustToolchain)) {
            throw @"
rustc is missing or cannot run at $rustc

Run once as Administrator on the Windows build machine:
  scripts\setup-windows-build.cmd
"@
        }
    }

    if (-not (Test-SystemRustExecutable $cargo)) {
        throw "cargo cannot run at $cargo"
    }

    Write-Host "==> RUSTUP_HOME=$($env:RUSTUP_HOME)"
    Write-Host "==> CARGO_HOME=$($env:CARGO_HOME)"
    Write-Host "==> rustc: $rustc"
    & $rustc --version
    Write-Host "==> cargo: $cargo"
    & $cargo --version
    & $rustup show
}

function Prioritize-SystemRustOnPath {
    param([switch]$ExportForCi)

    if (-not (Set-SystemRustEnv)) {
        return $false
    }

    $rustBin = Get-SystemRustCargoBin
    if (-not (Test-Path $rustBin)) {
        return $false
    }

    $rustc = Join-Path $rustBin "rustc.exe"
    $cargo = Join-Path $rustBin "cargo.exe"
    if (-not (Test-Path -LiteralPath $cargo)) {
        return $false
    }

    $block = Get-ChocolateyRustPathEntries
    $parts = @($env:PATH -split ';' | Where-Object {
            $_ -and ($_ -ne $rustBin) -and ($block -notcontains $_)
        })
    $env:PATH = ($rustBin + ';' + ($parts -join ';'))

    $rustc = Join-Path $rustBin "rustc.exe"
    $cargo = Join-Path $rustBin "cargo.exe"
    $env:RUSTC = $rustc
    $env:CARGO = $cargo

    if ($ExportForCi -and $env:GITHUB_ENV) {
        "PATH=$($env:PATH)" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
        "RUSTC=$rustc" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
        "CARGO=$cargo" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
        "CARGO_HOME=$($env:CARGO_HOME)" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
        "RUSTUP_HOME=$($env:RUSTUP_HOME)" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    }

    return $true
}

function Ensure-RustTarget {
    param([string]$Target)

    if (-not (Prioritize-SystemRustOnPath)) {
        Write-Error "Rust toolchain not found at C:\Rust - run scripts\setup-windows-build.cmd"
    }

    $rustBin = Get-SystemRustCargoBin
    $rustup = Join-Path $rustBin "rustup.exe"
    $rustc = Join-Path $rustBin "rustc.exe"

    Write-Host "==> RUSTUP_HOME=$($env:RUSTUP_HOME)"
    Write-Host "==> CARGO_HOME=$($env:CARGO_HOME)"
    Write-Host "==> rustc: $rustc"
    Write-Host "==> rustup target add $Target"
    & $rustup target add $Target
    if ($LASTEXITCODE -ne 0) {
        throw "rustup target add $Target failed (exit $LASTEXITCODE)"
    }

    $installed = @(& $rustup target list --installed)
    if ($installed -notcontains $Target) {
        & $rustup show
        throw "Rust target not installed: $Target"
    }

    $sysroot = (& $rustc --target $Target --print sysroot).Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($sysroot)) {
        & $rustup show
        throw "rustc sysroot missing for target $Target"
    }
    if ($sysroot -notlike "$($env:RUSTUP_HOME)*") {
        throw @"
Wrong rustc sysroot: $sysroot
Expected under RUSTUP_HOME=$($env:RUSTUP_HOME)
Chocolatey rust is shadowing C:\Rust - check PATH order.
"@
    }
    Write-Host "==> Rust sysroot ($Target): $sysroot"
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
    if (-not (Test-Path (Join-Path $cargoHome "bin\rustc.exe"))) {
        & (Join-Path $cargoHome "bin\rustup.exe") toolchain install stable --profile minimal -y *> $null
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
            (Get-SystemRustCargoBin),
            "${env:ProgramData}\chocolatey\bin",
            "${env:ProgramFiles}\Git\bin",
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

function Get-VsInstallPath {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { return $null }
    return & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath
}

function Ensure-VsCmakeOnPath {
    $installPath = Get-VsInstallPath
    if (-not $installPath) { return }

    foreach ($sub in @(
            "Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin",
            "Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja"
        )) {
        $dir = Join-Path $installPath $sub
        if (Test-Path $dir) {
            $env:PATH = "$dir;$env:PATH"
        }
    }

    $cmake = Get-Command cmake.exe -ErrorAction SilentlyContinue
    if ($cmake) {
        Write-Host "==> cmake: $($cmake.Source)"
    } else {
        Write-Warning "cmake.exe not found - install VS C++ CMake components"
    }
}

function Test-MsvcDevEnvironmentActive {
    return [bool]$env:INCLUDE -and [bool]$env:LIB -and
        (Get-Command cl.exe -ErrorAction SilentlyContinue) -and
        (Test-MsvcLinkerOnPath)
}

function Import-VsDevEnvironment {
    Remove-GitUsrBinFromPath

    if (Test-MsvcDevEnvironmentActive) {
        Write-Host "==> MSVC environment active"
        Write-Host "    cl:   $((Get-Command cl.exe).Source)"
        Write-Host "    link: $((Get-Command link.exe).Source)"
        Ensure-VsCmakeOnPath
        return
    }

    $installPath = Get-VsInstallPath
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

    if (-not (Test-MsvcDevEnvironmentActive)) {
        Write-Error "MSVC environment incomplete after VsDevCmd.bat (INCLUDE/LIB or cl.exe missing)"
    }
    Write-Host "==> MSVC cl:   $((Get-Command cl.exe).Source)"
    Write-Host "==> MSVC link: $((Get-Command link.exe).Source)"
    Ensure-VsCmakeOnPath
}

function Set-CmakeNinjaMsvcEnv {
    Import-VsDevEnvironment | Out-Null

    $cl = (Get-Command cl.exe -ErrorAction SilentlyContinue).Source
    if (-not $cl) {
        Write-Error "cl.exe not found for CMake"
    }

    $env:CC = $cl
    $env:CXX = $cl
    $env:CMAKE_GENERATOR = "Ninja"

    $ninja = Get-Command ninja.exe -ErrorAction SilentlyContinue
    if ($ninja) {
        $env:CMAKE_MAKE_PROGRAM = $ninja.Source
    } else {
        Write-Warning "ninja.exe not found on PATH"
    }

    $env:CMAKE_ARGS = "-DCMAKE_C_COMPILER=`"$cl`" -DCMAKE_CXX_COMPILER=`"$cl`" -DCMAKE_OBJECT_PATH_MAX=512"

    Write-Host "==> CMAKE_GENERATOR=Ninja"
    Write-Host "==> CC/CXX=$cl"
    if ($ninja) {
        Write-Host "==> CMAKE_MAKE_PROGRAM=$($ninja.Source)"
    }
}

function Get-ShortCargoTargetRoot {
  return "C:\ar\t"
}

function Get-CargoTargetRoot {
  if ($env:CARGO_TARGET_DIR) {
    return $env:CARGO_TARGET_DIR
  }
  return "target"
}

function Ensure-ShortBuildDirs {
  if (-not (Test-Admin)) { return }

  $root = Split-Path (Get-ShortCargoTargetRoot) -Parent
  $target = Get-ShortCargoTargetRoot
  New-Item -ItemType Directory -Force -Path $root, $target | Out-Null

  # Runner service (NETWORK SERVICE) must write CUDA/CMake artifacts here.
  Invoke-IcaclsGrant $root "NT AUTHORITY\NETWORK SERVICE:(OI)(CI)M"
  Invoke-IcaclsGrant $root "BUILTIN\Administrators:(OI)(CI)F"
  Write-Host "==> Short build dir ready: $target"
}

function Set-ShortCargoTargetDir {
  param([switch]$ExportForCi)

  $targetDir = Get-ShortCargoTargetRoot
  New-Item -ItemType Directory -Force -Path $targetDir -ErrorAction SilentlyContinue | Out-Null
  $env:CARGO_TARGET_DIR = $targetDir

  if ($ExportForCi -and $env:GITHUB_ENV) {
    "CARGO_TARGET_DIR=$targetDir" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
  }

  Write-Host "==> CARGO_TARGET_DIR=$targetDir"
}

function Set-WindowsBuildParallelism {
  param(
    [int]$Jobs = 4
  )

  $env:CARGO_BUILD_JOBS = "$Jobs"
  $env:CMAKE_BUILD_PARALLEL_LEVEL = "$Jobs"

  if ($env:GITHUB_ENV) {
    "CARGO_BUILD_JOBS=$Jobs" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    "CMAKE_BUILD_PARALLEL_LEVEL=$Jobs" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
  }

  Write-Host "==> Parallel build jobs: $Jobs"
}

function Clear-LlamaCmakeCache {
    $targetRoot = Get-CargoTargetRoot
    foreach ($profile in @("release\build", "x86_64-pc-windows-msvc\release\build")) {
        $root = Join-Path $targetRoot $profile
        if (-not (Test-Path $root)) { continue }
        Get-ChildItem $root -Directory -Filter "llama-cpp-sys-*" -ErrorAction SilentlyContinue |
            ForEach-Object {
                $cmakeDir = Join-Path $_.FullName "out\build"
                if (Test-Path $cmakeDir) {
                    Write-Host "==> Clearing stale CMake cache: $cmakeDir"
                    Remove-Item $cmakeDir -Recurse -Force
                }
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
