# Bootstrap PATH for self-hosted Windows GHA jobs (no bash required).
# Called from .github/workflows/release.yml build-windows-self-hosted.
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "windows-build-common.ps1")

$paths = Get-WindowsBuildPathEntries
if ($paths.Count -gt 0) {
    $env:PATH = (($paths -join ';') + ';' + $env:PATH)
}
Remove-GitUsrBinFromPath
Remove-ChocolateyRustFromPath

try {
    Assert-SystemRustToolchain -ExportForCi
} catch {
    Write-Error $_.Exception.Message
}

$clang = Find-LibClangDir
if (-not $clang) {
    Write-Error @"
libclang.dll not found (required by bindgen).

Run once as Administrator on the Windows build machine:
  scripts\setup-windows-build.cmd
"@
}
if ($env:GITHUB_ENV) {
    "LIBCLANG_PATH=$clang" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
}
$env:LIBCLANG_PATH = $clang
Write-Host "==> LIBCLANG_PATH=$clang"

Set-ShortCargoTargetDir -ExportForCi
Set-WindowsBuildParallelism -Jobs 4
