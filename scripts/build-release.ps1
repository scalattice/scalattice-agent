# Build and package scalattice-agent for Windows x86_64 (same output as CI).
#
# Usage (PowerShell):
#   ./scripts/build-release.ps1
#
# CI entry point for Windows releases (called from .github/workflows/release.yml).
# For local Windows builds: Rust stable, VS C++ tools, CUDA 12.6+, Inno Setup 6.
# Then copy dist/ScalatticeAgentSetup-x86_64.exe (+ zip) to your Linux release host.
param(
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$Features = "win-gpu",
    [string]$PackageVersion = ""
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
. (Join-Path $PSScriptRoot "windows-build-common.ps1")

$syncVersion = $PackageVersion
if (-not $syncVersion -and $env:SCALATTICE_VERSION) {
    $syncVersion = $env:SCALATTICE_VERSION.TrimStart('v')
}
if ($syncVersion) {
    & (Join-Path $PSScriptRoot "sync-cargo-version.ps1") -Version $syncVersion
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo not found - install Rust stable from https://rustup.rs"
}

Prioritize-SystemRustOnPath | Out-Null
Import-VsDevEnvironment
Prioritize-SystemRustOnPath | Out-Null
$env:TrackFileAccess = "false"

if (-not (Test-Path "Cargo.lock")) {
    Write-Host "==> Generating Cargo.lock"
    cargo generate-lockfile
}

Ensure-RustTarget -Target $Target

$env:CARGO_INCREMENTAL = "0"
if (-not $env:CUDA_PATH) {
    $defaultCuda = "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6"
    if (Test-Path $defaultCuda) {
        $env:CUDA_PATH = $defaultCuda
    }
}
if ($env:CUDA_PATH) {
    $env:PATH = "$($env:CUDA_PATH)\bin;$env:PATH"
}

$clang = Find-LibClangDir
if (-not $clang) {
    Write-Error "libclang.dll not found - run scripts\setup-windows-build.cmd"
}
$env:LIBCLANG_PATH = $clang

Set-CmakeNinjaMsvcEnv
Set-ShortCargoTargetDir
Set-WindowsBuildParallelism -Jobs 4

# win-gpu replaces default gpu (which includes vulkan); do not add to defaults
Write-Host "==> cargo build --release --target $Target --no-default-features --features $Features"
cargo build --release --target $Target --no-default-features --features $Features
if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo build failed with exit code $LASTEXITCODE"
}

$releaseDir = Join-Path (Get-CargoTargetRoot) (Join-Path $Target "release")
$bin = Join-Path $releaseDir "scalattice-agent.exe"
if (-not (Test-Path $bin)) {
    Write-Error "Missing binary: $bin"
}

New-Item -ItemType Directory -Force -Path "dist" | Out-Null
Copy-Item -LiteralPath $bin -Destination "dist\scalattice-agent.exe" -Force

$builtVersion = (& "dist\scalattice-agent.exe" --version 2>&1 | Out-String).Trim()
Write-Host "==> Built binary reports: $builtVersion"
if ($syncVersion -and $builtVersion -notmatch [regex]::Escape($syncVersion)) {
    Write-Error "Version mismatch: binary is '$builtVersion' but release tag is $syncVersion. Clear rust-cache and rebuild."
}

Copy-Item -LiteralPath (Join-Path $PSScriptRoot "..\installer\windows\scalattice-run.cmd") -Destination "dist\scalattice-run.cmd" -Force
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "..\installer\windows\launch-tray.vbs") -Destination "dist\launch-tray.vbs" -Force
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "..\installer\windows\launch-tray-interactive.vbs") -Destination "dist\launch-tray-interactive.vbs" -Force
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "..\installer\windows\launch-background.vbs") -Destination "dist\launch-background.vbs" -Force
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "..\installer\windows\open-tray-debug.cmd") -Destination "dist\open-tray-debug.cmd" -Force
& (Join-Path $PSScriptRoot "bundle-release-windows.ps1") -Binary "dist\scalattice-agent.exe" -OutDir "dist" -BuildRoot $releaseDir

. (Join-Path $PSScriptRoot "sign-artifact-windows.ps1")
Invoke-ScalatticeArtifactSigning -Paths @(
    (Join-Path (Get-Location) "dist\scalattice-agent.exe")
)

Write-Host ""
Write-Host "==> Bundled runtime libraries"
Get-ChildItem dist\lib -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    lib\$($_.Name)" }
Get-ChildItem dist\*.dll -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    $($_.Name)" }

if (-not $PackageVersion -and $env:SCALATTICE_VERSION) {
    $PackageVersion = $env:SCALATTICE_VERSION.TrimStart('v')
}

$archive = "dist\scalattice-agent-$Target.zip"
if (Test-Path $archive) { Remove-Item $archive -Force }

if (Test-Path "dist\lib") {
    Compress-Archive -Path "dist\scalattice-agent.exe", "dist\scalattice-run.cmd", "dist\launch-tray.vbs", "dist\launch-background.vbs", "dist\lib" -DestinationPath $archive -Force
} else {
    Compress-Archive -Path "dist\scalattice-agent.exe", "dist\scalattice-run.cmd", "dist\launch-tray.vbs", "dist\launch-background.vbs" -DestinationPath $archive -Force
}

Write-Host ""
Write-Host "==> Built $archive"
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::OpenRead((Resolve-Path $archive)).Entries | ForEach-Object { Write-Host "    $($_.FullName)" }

if (Get-Command choco -ErrorAction SilentlyContinue) {
    if (-not (Test-Path "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe") -and
        -not (Test-Path "${env:ProgramFiles}\Inno Setup 6\ISCC.exe")) {
        Write-Host "==> Installing Inno Setup (needed for ScalatticeAgentSetup-x86_64.exe)"
        choco install innosetup -y --no-progress | Out-Null
    }
}

if ((Test-Path "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe") -or (Test-Path "${env:ProgramFiles}\Inno Setup 6\ISCC.exe")) {
    $installerVersion = $PackageVersion
    if (-not $installerVersion -and $env:SCALATTICE_VERSION) {
        $installerVersion = $env:SCALATTICE_VERSION.TrimStart('v')
    }
    & (Join-Path $PSScriptRoot "build-windows-installer.ps1") -AppVersion $installerVersion
    Invoke-ScalatticeArtifactSigning -Paths @(
        (Join-Path (Get-Location) "dist\ScalatticeAgentSetup-x86_64.exe")
    )
} else {
    Write-Warning "Inno Setup not found - zip built but GUI installer skipped (install Inno Setup 6 and re-run)"
}
