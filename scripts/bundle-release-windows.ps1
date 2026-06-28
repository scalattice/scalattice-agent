# Bundle non-system DLLs so the agent runs without extra installs.
param(
    [Parameter(Mandatory = $true)][string]$Binary,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$BuildRoot = (Split-Path -Parent $Binary)
)

$ErrorActionPreference = "Stop"
$LibDir = Join-Path $OutDir "lib"
New-Item -ItemType Directory -Force -Path $LibDir | Out-Null

$SkipPattern = '(?i)(\\Windows\\|\\System32\\|\\SysWOW64\\|api-ms-win|ext-ms-win|vcruntime|msvcp|ucrtbase|kernel32|user32|advapi32|shell32|ole32|ws2_32|nvcuda\.dll|\\nvidia\\)'

function Find-Dumpbin {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { return $null }
    $install = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $install) { return $null }
    $dumpbin = Get-ChildItem -Path (Join-Path $install "VC\Tools\MSVC") -Recurse -Filter dumpbin.exe -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match 'Hostx64\\x64\\dumpbin\.exe$' } |
        Select-Object -First 1
    if ($dumpbin) { return $dumpbin.FullName }
    return $null
}

function Copy-DependencyTree {
    param([string]$Path)

    if (-not (Test-Path $Path)) { return }

    $dumpbin = Find-Dumpbin
    if (-not $dumpbin) {
        Write-Warning "dumpbin not found — skipping dependency scan for $Path"
        return
    }

    $output = & $dumpbin /nologo /dependents $Path 2>$null
    foreach ($line in $output) {
        $dll = $line.Trim()
        if ($dll -notmatch '\.dll$') { continue }
        if ($dll -match $SkipPattern) { continue }

        $resolved = $null
        if (Test-Path $dll) {
            $resolved = (Resolve-Path $dll).Path
        } else {
            $candidate = Join-Path (Split-Path -Parent $Path) $dll
            if (Test-Path $candidate) {
                $resolved = (Resolve-Path $candidate).Path
            }
        }
        if (-not $resolved) { continue }

        $dest = Join-Path $LibDir (Split-Path -Leaf $resolved)
        if (-not (Test-Path $dest)) {
            Copy-Item -LiteralPath $resolved -Destination $dest
            Copy-DependencyTree -Path $dest
        }
    }
}

Copy-DependencyTree -Path $Binary

if (Test-Path $BuildRoot) {
    Get-ChildItem -Path $BuildRoot -Recurse -Include "ggml*.dll", "llama*.dll" -ErrorAction SilentlyContinue |
        ForEach-Object {
            Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $LibDir $_.Name) -Force
            Copy-DependencyTree -Path $_.FullName
        }
}

if (-not (Get-ChildItem -Path $LibDir -ErrorAction SilentlyContinue)) {
    Remove-Item -Path $LibDir -Force -ErrorAction SilentlyContinue
}

Write-Host "Bundled libraries in $LibDir"
