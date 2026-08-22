# Bootstrap PATH for self-hosted Windows GHA jobs (no bash required).
# Called from .github/workflows/release.yml build-windows-self-hosted.
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "windows-build-common.ps1")

$paths = Get-WindowsBuildPathEntries
if ($paths.Count -gt 0) {
    $env:PATH = (($paths -join ';') + ';' + $env:PATH)
}
Remove-GitUsrBinFromPath
Ensure-ShortBuildDirsForCi
Ensure-RunnerRustToolchain -ExportForCi

if (-not (Prioritize-SystemRustOnPath -ExportForCi)) {
    Show-SystemRustDiagnostics
    Write-Error @"
Rust toolchain not available (checked C:\Rust and C:\ar\rust).

If this is the self-hosted runner, either:
  1. Run scripts\setup-windows-build.cmd as Administrator (preferred), or
  2. Re-run setup once so NETWORK SERVICE can write under C:\ar
"@
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo not found on PATH after Rust bootstrap"
}

$rustc = Get-SystemRustTool rustc
$cargo = Get-SystemRustTool cargo
$rustup = Get-SystemRustTool rustup

if ((Invoke-NativeTool $rustc --version) -ne 0) {
    throw "rustc failed version check: $rustc"
}
if ((Invoke-NativeTool $cargo --version) -ne 0) {
    throw "cargo failed version check: $cargo"
}
Invoke-NativeTool $rustup show | Out-Null

Ensure-RustTarget -Target x86_64-pc-windows-msvc

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
Clear-LlamaCmakeCache
