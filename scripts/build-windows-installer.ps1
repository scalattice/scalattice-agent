# Compile ScalatticeAgentSetup-x86_64.exe with Inno Setup 6.
param(
    # Avoid the name "Version" — PowerShell splatting can bind it incorrectly.
    [string]$AppVersion = "",
    [Alias("PackageVersion")]
    [string]$LegacyPackageVersion = ""
)

$ErrorActionPreference = "Stop"
$Root = Split-Path $PSScriptRoot -Parent
Set-Location $Root

if (-not $AppVersion -and $LegacyPackageVersion) {
    $AppVersion = $LegacyPackageVersion.TrimStart('v')
}

$distExe = Join-Path $Root "dist\scalattice-agent.exe"
if (-not (Test-Path $distExe)) {
    Write-Error "Missing $distExe - run scripts/build-release.ps1 first"
}

if (-not $AppVersion -and $env:SCALATTICE_VERSION) {
    $AppVersion = $env:SCALATTICE_VERSION.TrimStart('v')
}
if (-not $AppVersion) {
    $line = Select-String -Path (Join-Path $Root "Cargo.toml") -Pattern '^version = ' | Select-Object -First 1
    if ($line) {
        $AppVersion = ($line.Line -replace '^version = "(.*)"', '$1').Trim()
    }
}
if (-not $AppVersion) {
    Write-Error "Could not determine version (set SCALATTICE_VERSION or Cargo.toml version)"
}

$iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
if (-not (Test-Path $iscc)) {
    $iscc = "${env:ProgramFiles}\Inno Setup 6\ISCC.exe"
}
if (-not (Test-Path $iscc)) {
    Write-Error "Inno Setup 6 not found. Install: choco install innosetup -y"
}

$iss = Join-Path $Root "installer\windows\scalattice-agent.iss"
Write-Host "==> Compiling Windows setup (v$AppVersion)"
# Quoted define — unquoted /DMyAppVersion=1.0.21 can break; never pass a bare "-Version" token.
& $iscc "/DMyAppVersion=$AppVersion" $iss

$setup = Join-Path $Root "dist\ScalatticeAgentSetup-x86_64.exe"
if (-not (Test-Path $setup)) {
    Write-Error "Installer build failed - expected $setup"
}

Write-Host "==> Built $setup ($([math]::Round((Get-Item $setup).Length / 1MB, 1)) MB)"
