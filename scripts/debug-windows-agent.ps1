# Quick diagnostics when the Windows agent appears to do nothing.
param(
    [switch]$LaunchTrayVisible
)

$ErrorActionPreference = "Continue"
$bin = Join-Path $env:LOCALAPPDATA "Scalattice\bin"
$lib = Join-Path $env:LOCALAPPDATA "Scalattice\lib"
$logs = Join-Path $env:LOCALAPPDATA "Scalattice\logs"
$run = Join-Path $bin "scalattice-run.cmd"
$exe = Join-Path $bin "scalattice-agent.exe"

Write-Host "==> Scalattice Agent diagnostics"
Write-Host "    bin:  $bin"
Write-Host "    lib:  $lib"
Write-Host "    logs: $logs"
Write-Host ""

function Step($label, [scriptblock]$block) {
    Write-Host "==> $label"
    & $block
    Write-Host ""
}

Step "Installed files" {
    foreach ($name in @(
        "scalattice-agent.exe", "scalattice-run.cmd",
        "launch-tray.vbs", "launch-background.vbs", "run-background.cmd"
    )) {
        $path = Join-Path $bin $name
        Write-Host ("    {0,-24} {1}" -f $name, ($(if (Test-Path $path) { "OK" } else { "MISSING" })))
    }
}

Step "CUDA / bundled DLLs" {
    foreach ($name in @("cudart64_12.dll", "cublas64_12.dll", "cublasLt64_12.dll")) {
        $path = Join-Path $lib $name
        Write-Host ("    {0,-24} {1}" -f $name, ($(if (Test-Path $path) { "OK" } else { "MISSING" })))
    }
}

Step "Running processes" {
    Get-Process scalattice-agent -ErrorAction SilentlyContinue |
        Format-Table Id, StartTime, Path -AutoSize |
        Out-String |
        ForEach-Object { $_.TrimEnd() } |
        ForEach-Object { Write-Host $_ }
    if (-not (Get-Process scalattice-agent -ErrorAction SilentlyContinue)) {
        Write-Host "    (no scalattice-agent.exe processes)"
    }
}

Step "Token config" {
    $envFile = Join-Path $env:USERPROFILE ".config\scalattice\agent.env"
    if (Test-Path $envFile) {
        Write-Host "    agent.env exists"
        Select-String -Path $envFile -Pattern '^SCALATTICE_AGENT_TOKEN=' -Quiet | ForEach-Object {
            if ($_) { Write-Host "    token: set" } else { Write-Host "    token: missing in file" }
        }
    } else {
        Write-Host "    agent.env MISSING"
    }
}

Step "Autostart shortcuts" {
    $startup = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\Startup"
    foreach ($name in @("ScalatticeAgent.vbs", "ScalatticeAgentTray.vbs")) {
        $path = Join-Path $startup $name
        Write-Host ("    {0,-24} {1}" -f $name, ($(if (Test-Path $path) { "OK" } else { "missing" })))
    }
}

Step "Scheduled tasks" {
    foreach ($tn in @("ScalatticeAgent", "ScalatticeAgentTray")) {
        $q = schtasks /Query /TN $tn 2>$null
        Write-Host ("    {0,-24} {1}" -f $tn, ($(if ($LASTEXITCODE -eq 0) { "registered" } else { "missing" })))
    }
}

Step "Recent logs" {
    foreach ($name in @("agent.log", "tray.log")) {
        $path = Join-Path $logs $name
        Write-Host "    --- $name ---"
        if (Test-Path $path) {
            Get-Content $path -Tail 20 | ForEach-Object { Write-Host "    $_" }
        } else {
            Write-Host "    (not created yet)"
        }
    }
}

Step "CLI status (via scalattice-run.cmd)" {
    if (Test-Path $run) {
        & $run status
    } else {
        Write-Host "    scalattice-run.cmd missing — cannot run status"
    }
}

    if ($LaunchTrayVisible) {
    Step "Launch tray with visible console (errors print here)" {
        if (-not (Test-Path $exe)) {
            Write-Host "    exe missing"
            return
        }
        $env:PATH = "$bin;$lib;$env:PATH"
        Remove-Item Env:SCALATTICE_TRAY_HIDDEN -ErrorAction SilentlyContinue
        Push-Location $bin
        try {
            & $exe tray --force
        } finally {
            Pop-Location
        }
    }
} else {
    Write-Host "Tip: re-run with -LaunchTrayVisible to start the tray UI in this window and see startup errors."
    Write-Host "Tip: after clicking the notification icon, check $logs\tray.log"
}
