# Set package version in Cargo.toml (used by CI so the built binary matches the release tag).
param(
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"
$Root = Split-Path $PSScriptRoot -Parent
Set-Location $Root

if (-not $Version -and $env:SCALATTICE_VERSION) {
    $Version = $env:SCALATTICE_VERSION.TrimStart('v')
}
if (-not $Version) {
    Write-Error "No version: pass -Version or set SCALATTICE_VERSION (e.g. v1.0.22)"
}

$cargo = Join-Path $Root "Cargo.toml"
$text = Get-Content -LiteralPath $cargo -Raw
if ($text -notmatch '(?m)^version = "[^"]+"') {
    Write-Error "Could not find version = in Cargo.toml"
}
$updated = [regex]::Replace($text, '(?m)^version = "[^"]+"', "version = `"$Version`"", 1)
if ($updated -eq $text) {
    Write-Host "==> Cargo.toml already at v$Version"
} else {
    Set-Content -LiteralPath $cargo -Value $updated -NoNewline
    Write-Host "==> Cargo.toml set to v$Version"
}

function Resolve-CargoExe {
    $common = Join-Path $PSScriptRoot "windows-build-common.ps1"
    if (Test-Path -LiteralPath $common) {
        . $common
        $paths = Get-WindowsBuildPathEntries
        if ($paths.Count -gt 0) {
            $env:PATH = (($paths -join ';') + ';' + $env:PATH)
        }
        Ensure-RunnerRustToolchain | Out-Null
        if (Prioritize-SystemRustOnPath) {
            try {
                return (Get-SystemRustTool cargo)
            } catch {
                Write-Host "==> System Rust bootstrap incomplete: $_"
            }
        }
    }

    if ($env:CARGO -and (Test-Path -LiteralPath $env:CARGO)) {
        return $env:CARGO
    }

    $cmd = Get-Command cargo -ErrorAction SilentlyContinue
    if ($cmd -and $cmd.Source -and (Test-Path -LiteralPath $cmd.Source)) {
        return $cmd.Source
    }

    return $null
}

# Keep Cargo.lock in sync when a working cargo is available.
$cargoExe = Resolve-CargoExe
if (-not $cargoExe) {
    Write-Host "==> Skipping Cargo.lock sync (cargo not on PATH yet)"
} else {
    try {
        & $cargoExe --version | Out-Host
        & $cargoExe generate-lockfile | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "cargo generate-lockfile exited $LASTEXITCODE (continuing)"
        } else {
            Write-Host "==> Cargo.lock refreshed"
        }
    } catch {
        Write-Warning "cargo generate-lockfile failed: $_ (continuing)"
    }
}

Write-Host "==> Verified: $(Select-String -Path $cargo -Pattern '^version = ' | Select-Object -First 1 -ExpandProperty Line)"
