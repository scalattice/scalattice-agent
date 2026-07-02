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
    [string]$Repo = "scalattice/scalattice-agent",
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

function Get-RunnerRoot {
    param([string]$Dir)

    if (Test-Path (Join-Path $Dir "config.cmd")) {
        return (Resolve-Path $Dir).Path
    }

    $nested = Get-ChildItem -Path $Dir -Directory -ErrorAction SilentlyContinue |
        Where-Object { Test-Path (Join-Path $_.FullName "config.cmd") } |
        Select-Object -First 1
    if ($nested) {
        return $nested.FullName
    }

    return $null
}

function Install-RunnerPackage {
    param(
        [string]$Dir,
        [string]$Version
    )

    $zipName = "actions-runner-win-x64-$Version.zip"
    $downloadUrl = "https://github.com/actions/runner/releases/download/v$Version/$zipName"

    Write-Host "==> Downloading actions-runner $Version"
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $zipPath = Join-Path $Dir $zipName
    Invoke-WebRequest -Uri $downloadUrl -OutFile $zipPath
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::ExtractToDirectory($zipPath, $Dir)
    Remove-Item $zipPath -Force
}

function Ensure-RunnerPackage {
    param(
        [string]$Dir,
        [string]$Version
    )

    $root = Get-RunnerRoot -Dir $Dir
    if ($root -and (Test-Path (Join-Path $root "run.cmd"))) {
        return $root
    }

    if ($root) {
        Write-Host "==> Runner install at $root is incomplete; re-downloading"
    }

    if (Test-Path $Dir) {
        Get-ChildItem -Path $Dir -Force | Remove-Item -Recurse -Force
    }

    Install-RunnerPackage -Dir $Dir -Version $Version
    $root = Get-RunnerRoot -Dir $Dir
    if (-not $root) {
        Write-Error "Runner package extracted but config.cmd was not found under $Dir"
    }
    return $root
}

function Get-RunnerService {
    return @(Get-Service -Name "actions.runner.*" -ErrorAction SilentlyContinue)
}

function Start-RunnerService {
    param([string]$RunnerRoot)

    $services = Get-RunnerService
    if ($services.Count -gt 0) {
        foreach ($svc in $services) {
            if ($svc.Status -ne "Running") {
                Write-Host "==> Starting service $($svc.Name)"
                Start-Service $svc.Name
            }
        }
        return
    }

    $svcCmd = Join-Path $RunnerRoot "svc.cmd"
    if (Test-Path $svcCmd) {
        Write-Host "==> Installing and starting Windows service (svc.cmd)"
        & $svcCmd install
        & $svcCmd start
        return
    }

    $runnerService = Join-Path $RunnerRoot "bin\RunnerService.exe"
    if (Test-Path $runnerService) {
        Write-Host "==> Installing and starting Windows service (RunnerService.exe)"
        & $runnerService install
        & $runnerService start
        return
    }

    Write-Warning "No runner Windows service found. Start interactively with: $RunnerRoot\run.cmd"
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
$runnerRoot = Ensure-RunnerPackage -Dir $WorkDir -Version $runnerVersion
Set-Location $runnerRoot

$existing = Get-RegisteredRunnerId -Repository $Repo -Name $RunnerName
if ($existing) {
    Write-Host "==> Removing existing runner registration for $RunnerName"
    $removeToken = gh api --method POST "repos/$Repo/actions/runners/remove-token" -q .token
    & .\config.cmd remove --token $removeToken 2>$null
}

Write-Host "==> Configuring runner '$RunnerName' with labels: $Labels"
# On Windows, the service is installed during config (--runasservice), not via svc.cmd.
& .\config.cmd `
    --url "https://github.com/$Repo" `
    --token $RegistrationToken `
    --name $RunnerName `
    --labels $Labels `
    --unattended `
    --replace `
    --runasservice `
    --windowslogonaccount "NT AUTHORITY\NETWORK SERVICE"

Start-RunnerService -RunnerRoot $runnerRoot

$services = Get-RunnerService
if ($services.Count -gt 0) {
    foreach ($svc in $services) {
        Write-Host "==> Runner service: $($svc.Name) ($($svc.Status))"
    }
} else {
    Write-Warning "Runner registered but no Windows service is running yet."
    Write-Host "    Start manually: $runnerRoot\run.cmd"
}

Write-Host ""
Write-Host "==> Self-hosted runner online. From Linux release host:"
Write-Host "    ./scripts/release.sh --dev"
