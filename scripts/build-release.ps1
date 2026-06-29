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
    [string]$Features = "win-gpu"
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
. (Join-Path $PSScriptRoot "windows-build-common.ps1")

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo not found - install Rust stable from https://rustup.rs"
}

Prioritize-SystemRustOnPath | Out-Null
Import-VsDevEnvironment
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

# win-gpu replaces default gpu (which includes vulkan); do not add to defaults
Write-Host "==> cargo build --release --target $Target --no-default-features --features $Features"
cargo build --release --target $Target --no-default-features --features $Features

$releaseDir = Join-Path "target" (Join-Path $Target "release")
$bin = Join-Path $releaseDir "scalattice-agent.exe"
if (-not (Test-Path $bin)) {
    Write-Error "Missing binary: $bin"
}

New-Item -ItemType Directory -Force -Path "dist" | Out-Null
Copy-Item -LiteralPath $bin -Destination "dist\scalattice-agent.exe" -Force
& (Join-Path $PSScriptRoot "bundle-release-windows.ps1") -Binary "dist\scalattice-agent.exe" -OutDir "dist" -BuildRoot $releaseDir

$archive = "dist\scalattice-agent-$Target.zip"
if (Test-Path $archive) { Remove-Item $archive -Force }

if (Test-Path "dist\lib") {
    Compress-Archive -Path "dist\scalattice-agent.exe", "dist\lib" -DestinationPath $archive -Force
} else {
    Compress-Archive -Path "dist\scalattice-agent.exe" -DestinationPath $archive -Force
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
    & (Join-Path $PSScriptRoot "build-windows-installer.ps1")
} else {
    Write-Warning "Inno Setup not found - zip built but GUI installer skipped (install Inno Setup 6 and re-run)"
}
