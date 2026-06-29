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

# Keep Cargo.lock in sync when present.
if (Get-Command cargo -ErrorAction SilentlyContinue) {
    cargo generate-lockfile | Out-Null
}

Write-Host "==> Verified: $(Select-String -Path $cargo -Pattern '^version = ' | Select-Object -First 1 -ExpandProperty Line)"
