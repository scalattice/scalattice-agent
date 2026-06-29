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
        Write-Warning "dumpbin not found - skipping dependency scan for $Path"
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

function Copy-CudaRuntimeLibs {
    param(
        [string]$DestDir,
        [string]$AlsoDestDir = ""
    )

    $cuda = $env:CUDA_PATH
    if (-not $cuda) {
        $cuda = "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6"
    }
    $cudaBin = Join-Path $cuda "bin"
    if (-not (Test-Path $cudaBin)) {
        Write-Warning "CUDA bin not found at $cudaBin - CUDA runtime DLLs not bundled"
        return @()
    }

    # ggml-cuda loads these at runtime; dumpbin does not see them on the main exe.
    $patterns = @(
        "cudart64_12.dll",
        "cublas64_12.dll",
        "cublasLt64_12.dll",
        "nvrtc-builtins64_12.dll",
        "nvrtc64_12*.dll"
    )

    $bundled = @()
    foreach ($pattern in $patterns) {
        Get-ChildItem -Path $cudaBin -Filter $pattern -ErrorAction SilentlyContinue | ForEach-Object {
            $dest = Join-Path $DestDir $_.Name
            Copy-Item -LiteralPath $_.FullName -Destination $dest -Force
            Copy-DependencyTree -Path $dest
            $bundled += $_.Name
            Write-Host "    bundled $($_.Name)"

            if ($AlsoDestDir) {
                Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $AlsoDestDir $_.Name) -Force
            }
        }
    }

    if ($bundled.Count -eq 0) {
        Write-Warning "No CUDA 12 runtime DLLs found under $cudaBin"
    }
    return $bundled | Select-Object -Unique
}

$requiredCuda = @("cudart64_12.dll", "cublas64_12.dll", "cublasLt64_12.dll")
$cudaBundled = Copy-CudaRuntimeLibs -DestDir $LibDir -AlsoDestDir $OutDir
$missingCuda = @($requiredCuda | Where-Object { $_ -notin $cudaBundled })
if ($missingCuda.Count -gt 0) {
    $cudaRoot = $env:CUDA_PATH
    if (-not $cudaRoot) {
        $cudaRoot = "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6"
    }
    throw @"
CUDA runtime DLLs missing from bundle: $($missingCuda -join ', ')

The Windows build machine needs CUDA 12.6 toolkit installed (not just the NVIDIA driver).
Expected under: $(Join-Path $cudaRoot 'bin')
"@
}

if (-not (Get-ChildItem -Path $LibDir -ErrorAction SilentlyContinue)) {
    Remove-Item -Path $LibDir -Force -ErrorAction SilentlyContinue
}

Write-Host "Bundled libraries in $LibDir"
