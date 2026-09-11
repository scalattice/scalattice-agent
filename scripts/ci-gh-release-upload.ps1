# Attach already-built archives to a GitHub release, retrying on reset/timeout.
# Used by the Windows release job so a 1.5GB Actions artifact finalize cannot
# swallow a finished build (ECONNRESET on FinalizeArtifact).
param(
    [Parameter(Mandatory = $true)][string]$Tag,
    [Parameter(Mandatory = $true)][string[]]$Files
)

$ErrorActionPreference = "Stop"
$repo = if ($env:GH_REPO) { $env:GH_REPO } else { "scalattice/scalattice-agent" }

foreach ($f in $Files) {
    if (-not (Test-Path -LiteralPath $f)) {
        throw "Missing release asset $f"
    }
}

$max = 5
for ($n = 1; $n -le $max; $n++) {
    & gh release upload $Tag @Files --clobber -R $repo
    if ($LASTEXITCODE -eq 0) {
        Write-Host "==> attached $($Files.Count) asset(s) to $Tag"
        exit 0
    }
    if ($n -eq $max) {
        throw "gh release upload failed after $max attempts"
    }
    $delay = 20 * $n
    Write-Host "==> gh release upload failed (attempt $n/$max), retrying in ${delay}s"
    Start-Sleep -Seconds $delay
}
