# Ensure a real CPython, then run the update smoke.
# Self-hosted Windows runners often have no Python on PATH, and
# actions/setup-python tries a full installer (registry + hostedtoolcache)
# which fails without admin. The official embeddable zip is enough: the
# smoke uses only the stdlib.
param(
    [string]$Dist = "dist"
)

$ErrorActionPreference = "Stop"

function Test-RealPython {
    param([string]$Exe)
    if (-not $Exe) { return $false }
    if (-not (Test-Path -LiteralPath $Exe)) { return $false }
    if ($Exe -match "WindowsApps") { return $false }
    try {
        $null = & $Exe -c "import sys" 2>$null
        return ($LASTEXITCODE -eq 0)
    } catch {
        return $false
    }
}

function Find-ExistingPython {
    foreach ($name in @("python", "python3")) {
        $cmd = Get-Command $name -ErrorAction SilentlyContinue
        if ($cmd -and (Test-RealPython $cmd.Source)) {
            return $cmd.Source
        }
    }
    $py = Get-Command py -ErrorAction SilentlyContinue
    if ($py) {
        try {
            $exe = & py -3 -c "import sys; print(sys.executable)" 2>$null
            if ($LASTEXITCODE -eq 0 -and (Test-RealPython "$exe".Trim())) {
                return "$exe".Trim()
            }
        } catch {}
    }
    $candidates = @(
        "$env:LocalAppData\Programs\Python\Python312\python.exe",
        "$env:LocalAppData\Programs\Python\Python311\python.exe",
        "$env:ProgramFiles\Python312\python.exe",
        "$env:ProgramFiles\Python311\python.exe",
        "C:\Python312\python.exe",
        "C:\Python311\python.exe"
    )
    foreach ($exe in $candidates) {
        if (Test-RealPython $exe) { return $exe }
    }
    return $null
}

function Install-EmbeddablePython {
    $root = $env:RUNNER_TEMP
    if (-not $root) { $root = $env:TEMP }
    $dest = Join-Path $root "scalattice-python-embed-3.12.10"
    $exe = Join-Path $dest "python.exe"
    if (Test-RealPython $exe) {
        return $exe
    }
    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    $zip = Join-Path $root "python-3.12.10-embed-amd64.zip"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $url = "https://www.python.org/ftp/python/3.12.10/python-3.12.10-embed-amd64.zip"
    Write-Host "==> downloading embeddable CPython 3.12.10 (no installer / registry)"
    try {
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    } catch {
        throw "Could not download embeddable CPython from python.org: $($_.Exception.Message). Install Python 3.12 on the runner, or allow that download."
    }
    if (Get-Command Expand-Archive -ErrorAction SilentlyContinue) {
        Expand-Archive -LiteralPath $zip -DestinationPath $dest -Force
    } else {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        [System.IO.Compression.ZipFile]::ExtractToDirectory($zip, $dest)
    }
    if (-not (Test-RealPython $exe)) {
        throw "embeddable python extracted but $exe does not run"
    }
    return $exe
}

$python = Find-ExistingPython
if (-not $python) {
    $python = Install-EmbeddablePython
}
Write-Host "==> python $python"
$smoke = Join-Path $PSScriptRoot "ci-update-smoke.py"
$distPath = $Dist
if (-not [System.IO.Path]::IsPathRooted($distPath)) {
    $distPath = Join-Path (Get-Location) $Dist
}
$env:PYTHONUNBUFFERED = "1"
$env:PYTHONIOENCODING = "utf-8"
& $python -u $smoke --dist $distPath
exit $LASTEXITCODE
