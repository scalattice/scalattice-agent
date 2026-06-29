# Bootstrap PATH for self-hosted Windows GHA jobs (no bash required).
# Called from .github/workflows/release.yml build-windows-self-hosted.
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "windows-build-common.ps1")

$paths = Get-WindowsBuildPathEntries
if ($paths.Count -gt 0) {
    $extra = ($paths -join ';')
    if ($env:GITHUB_PATH) {
        "PATH=$extra;$env:PATH" | Out-File -FilePath $env:GITHUB_PATH -Append -Encoding utf8
    }
    $env:PATH = "$extra;$env:PATH"
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error @"
cargo not found on this self-hosted runner.

Run once as Administrator on the Windows build machine:
  scripts\setup-windows-build.cmd
"@
}

rustc --version
cargo --version

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
