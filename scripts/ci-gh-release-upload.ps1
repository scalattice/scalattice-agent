# Attach already-built archives to a GitHub release, one file at a time.
# A full-speed upload of the Windows zip (~1.2 GB) plus the installer saturates
# a home uplink and reboots some routers. Cap the rate and do not retry in a
# burst: a failed attempt must leave the line idle long enough for the router
# to stay up.
param(
    [Parameter(Mandatory = $true)][string]$Tag,
    [Parameter(Mandatory = $true)][string[]]$Files
)

$ErrorActionPreference = "Stop"
$repo = if ($env:GH_REPO) { $env:GH_REPO } else { "scalattice/scalattice-agent" }
# Default is the home-PC cap (~2.4 Mbit/s). The GitHub-hosted job sets
# SCALATTICE_RELEASE_UPLOAD_RATE=0, which means no curl rate limit.
$rate = if ($env:SCALATTICE_RELEASE_UPLOAD_RATE) { $env:SCALATTICE_RELEASE_UPLOAD_RATE } else { "300K" }
$unlimited = $rate -eq "0" -or $rate -eq "unlimited" -or $rate -eq "none"

if (-not $env:GH_TOKEN) {
    throw "GH_TOKEN is required"
}

foreach ($f in $Files) {
    if (-not (Test-Path -LiteralPath $f)) {
        throw "Missing release asset $f"
    }
}

$release = gh api "repos/$repo/releases/tags/$Tag" | ConvertFrom-Json
if (-not $release.id) {
    throw "Release $Tag not found on $repo"
}

function Upload-One([int64]$ReleaseId, [string]$Path) {
    $name = [IO.Path]::GetFileName($Path)
    $bytes = (Get-Item -LiteralPath $Path).Length
    $existing = gh api "repos/$repo/releases/$ReleaseId/assets" --jq ".[] | select(.name==`"$name`") | .id"
    foreach ($id in @($existing)) {
        if (-not $id) { continue }
        Write-Host "==> replacing existing asset $name ($id)"
        gh api --method DELETE "repos/$repo/releases/assets/$id" | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw "failed to delete existing asset $name"
        }
    }

    $url = "https://uploads.github.com/repos/$repo/releases/$ReleaseId/assets?name=$([uri]::EscapeDataString($name))"
    $pace = if ($unlimited) { "full speed" } else { $rate }
    Write-Host "==> uploading $name ($bytes bytes) at $pace"
    $curlArgs = @(
        "--http1.1", "--fail-with-body", "--show-error", "--silent",
        "--connect-timeout", "30", "--max-time", "10800"
    )
    if (-not $unlimited) {
        $curlArgs += @("--limit-rate", $rate)
    }
    $curlArgs += @(
        "-X", "POST",
        "-H", "Authorization: Bearer $($env:GH_TOKEN)",
        "-H", "Accept: application/vnd.github+json",
        "-H", "Content-Type: application/octet-stream",
        "-T", $Path,
        $url
    )
    & curl.exe @curlArgs
    if ($LASTEXITCODE -ne 0) {
        throw "upload failed for $name (curl exit $LASTEXITCODE)"
    }
    Write-Host "==> uploaded $name"
}

foreach ($f in $Files) {
    $max = 2
    for ($n = 1; $n -le $max; $n++) {
        try {
            Upload-One -ReleaseId $release.id -Path $f
            break
        } catch {
            if ($n -eq $max) { throw }
            Write-Host "==> $($_.Exception.Message)"
            Write-Host "==> waiting 3 minutes before one more try so the uplink can recover"
            Start-Sleep -Seconds 180
        }
    }
}

$pace = if ($unlimited) { "full speed" } else { $rate }
Write-Host "==> attached $($Files.Count) asset(s) to $Tag at $pace"
