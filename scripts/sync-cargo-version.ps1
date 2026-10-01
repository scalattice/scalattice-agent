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

# Patch only the workspace package version in Cargo.lock. Never run
# `cargo generate-lockfile` here — that re-resolves to latest compatible
# crates (e.g. llama-cpp-2 0.1.154 → 0.1.158) and breaks the Windows build.
$lock = Join-Path $Root "Cargo.lock"
if (Test-Path -LiteralPath $lock) {
    $lockText = Get-Content -LiteralPath $lock -Raw
    $updatedLock = [regex]::Replace(
        $lockText,
        '(?ms)(name = "scalattice-agent"\r?\n)version = "[^"]+"',
        "`${1}version = `"$Version`"",
        1
    )
    if ($updatedLock -eq $lockText) {
        Write-Host "==> Cargo.lock already at v$Version (or package entry not found)"
    } else {
        Set-Content -LiteralPath $lock -Value $updatedLock -NoNewline
        Write-Host "==> Cargo.lock package version set to v$Version"
    }
} else {
    Write-Host "==> No Cargo.lock to patch"
}

Write-Host "==> Verified: $(Select-String -Path $cargo -Pattern '^version = ' | Select-Object -First 1 -ExpandProperty Line)"
