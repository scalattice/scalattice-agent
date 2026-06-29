# Register this Windows machine as a GitHub Actions self-hosted runner for Scalattice releases.
# Requires: Admin, gh CLI logged in (gh auth login), setup-windows-build done.
#
# Usage (Admin cmd or PowerShell):
#   scripts\install-windows-runner.cmd
#
# Or PowerShell:
#   Set-ExecutionPolicy Bypass -Scope Process -Force
#   .\scripts\install-windows-runner.ps1

param(
    [string]$Repo = "Robottik-Software/Scalattice-Client",
    [string]$RunnerName = $env:COMPUTERNAME,
    [string]$Labels = "self-hosted,Windows,X64,scalattice-release",
    [string]$WorkDir = "C:\actions-runner-scalattice",
    [string]$RegistrationToken = ""
)

$ErrorActionPreference = "Stop"

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $p = New-Object Security.Principal.WindowsPrincipal($id)
    return $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-RegisteredRunnerId {
    param(
        [string]$Repository,
        [string]$Name
    )
    $json = gh api "repos/$Repository/actions/runners" 2>$null
    if (-not $json) { return $null }
    $data = $json | ConvertFrom-Json
    $match = @($data.runners | Where-Object { $_.name -eq $Name }) | Select-Object -First 1
    if ($match) { return $match.id }
    return $null
}

if (-not (Test-Admin)) {
    Write-Error "Run as Administrator."
}

if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    Write-Error "Install GitHub CLI and run: gh auth login"
}

if (-not $RegistrationToken) {
    Write-Host "==> Fetching runner registration token for $Repo"
    $RegistrationToken = gh api --method POST "repos/$Repo/actions/runners/registration-token" -q .token
    if (-not $RegistrationToken) {
        Write-Error "Could not get registration token - check gh auth and repo access"
    }
}

$runnerVersion = (gh api repos/actions/runner/releases/latest -q .tag_name).TrimStart('v')
$zipName = "actions-runner-win-x64-$runnerVersion.zip"
$downloadUrl = "https://github.com/actions/runner/releases/download/v$runnerVersion/$zipName"

New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
Set-Location $WorkDir

if (-not (Test-Path ".\config.cmd")) {
    Write-Host "==> Downloading actions-runner $runnerVersion"
    Invoke-WebRequest -Uri $downloadUrl -OutFile $zipName
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::ExtractToDirectory((Resolve-Path $zipName), $WorkDir)
    Remove-Item $zipName -Force
}

$existing = Get-RegisteredRunnerId -Repository $Repo -Name $RunnerName
if ($existing) {
    Write-Host "==> Removing existing runner registration for $RunnerName"
    $removeToken = gh api --method POST "repos/$Repo/actions/runners/remove-token" -q .token
    & .\config.cmd remove --token $removeToken 2>$null
}

Write-Host "==> Configuring runner '$RunnerName' with labels: $Labels"
& .\config.cmd `
    --url "https://github.com/$Repo" `
    --token $RegistrationToken `
    --name $RunnerName `
    --labels $Labels `
    --unattended `
    --replace

Write-Host "==> Installing and starting Windows service"
& .\svc.cmd install
& .\svc.cmd start

Write-Host ""
Write-Host "==> Self-hosted runner online. From Linux release host:"
Write-Host "    ./scripts/release.sh --dev"
