# Compile ScalatticeAgentSetup-x86_64.exe with Inno Setup 6.
param(
    [string]$Version = "",
    [string]$PackageVersion = ""
)

$ErrorActionPreference = "Stop"
$Root = Split-Path $PSScriptRoot -Parent
Set-Location $Root

if (-not $Version -and $PackageVersion) {
    $Version = $PackageVersion.TrimStart('v')
}

$distExe = Join-Path $Root "dist\scalattice-agent.exe"
if (-not (Test-Path $distExe)) {
    Write-Error "Missing $distExe - run scripts/build-release.ps1 first"
}

if (-not $Version -and $env:SCALATTICE_VERSION) {
    $Version = $env:SCALATTICE_VERSION.TrimStart('v')
}
if (-not $Version) {
    $line = Select-String -Path (Join-Path $Root "Cargo.toml") -Pattern '^version = ' | Select-Object -First 1
    if ($line) {
        $Version = ($line.Line -replace '^version = "(.*)"', '$1').Trim()
    }
}
if (-not $Version) {
    Write-Error "Could not determine version (set SCALATTICE_VERSION or Cargo.toml version)"
}

$iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
if (-not (Test-Path $iscc)) {
    $iscc = "${env:ProgramFiles}\Inno Setup 6\ISCC.exe"
}
if (-not (Test-Path $iscc)) {
    Write-Error "Inno Setup 6 not found. Install: choco install innosetup -y"
}

Write-Host "==> Compiling Windows setup (v$Version)"
& $iscc "/DMyAppVersion=$Version" (Join-Path $Root "installer\windows\scalattice-agent.iss")

$setup = Join-Path $Root "dist\ScalatticeAgentSetup-x86_64.exe"
if (-not (Test-Path $setup)) {
    Write-Error "Installer build failed - expected $setup"
}

Write-Host "==> Built $setup ($([math]::Round((Get-Item $setup).Length / 1MB, 1)) MB)"
