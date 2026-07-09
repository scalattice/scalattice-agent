# Ensure Azure Artifact Signing Client Tools are available for SignTool.
# Skips install when Azure.CodeSigning.Dlib.dll is already on the machine.
param(
    [switch]$Quiet
)

$ErrorActionPreference = 'Stop'

function Write-InstallLog {
    param([string]$Message)
    if (-not $Quiet) {
        Write-Host $Message
    }
}

function Get-ArtifactSigningDlibSearchRoots {
    $roots = @(
        $env:SCALATTICE_SIGN_DLIB_DIR,
        "${env:LOCALAPPDATA}\Microsoft\ArtifactSigningClientTools",
        "${env:LOCALAPPDATA}\Microsoft\ArtifactSigningTools",
        "${env:ProgramFiles}\Microsoft\Azure Artifact Signing Client Tools",
        "${env:ProgramFiles}\Microsoft Artifact Signing Client Tools",
        "${env:ProgramFiles}\Microsoft\ArtifactSigningClientTools",
        "${env:ProgramFiles(x86)}\Microsoft\Azure Artifact Signing Client Tools",
        "${env:ProgramFiles(x86)}\Microsoft\ArtifactSigningClientTools",
        (Join-Path $PSScriptRoot '..\.tools')
    ) | Where-Object { $_ -and (Test-Path $_) }
    return $roots
}

function Find-ArtifactSigningDlib {
    if ($env:SCALATTICE_SIGN_DLIB -and (Test-Path $env:SCALATTICE_SIGN_DLIB)) {
        return (Resolve-Path $env:SCALATTICE_SIGN_DLIB).Path
    }

    $fileName = 'Azure.CodeSigning.Dlib.dll'
    foreach ($root in Get-ArtifactSigningDlibSearchRoots) {
        $match = Get-ChildItem $root -Recurse -Filter $fileName -ErrorAction SilentlyContinue |
            Sort-Object { if ($_.FullName -match '\\x64\\') { 0 } elseif ($_.FullName -match '\\bin\\') { 1 } else { 2 } } |
            Select-Object -First 1
        if ($match) {
            return $match.FullName
        }
    }
    return $null
}

function Publish-ArtifactSigningDlibPath {
    param([Parameter(Mandatory = $true)][string]$Path)
    $env:SCALATTICE_SIGN_DLIB = $Path
    if ($env:GITHUB_ENV) {
        "SCALATTICE_SIGN_DLIB=$Path" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    }
}

function Install-ArtifactSigningDlibFromNuGet {
    $repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
    $toolsDir = Join-Path $repoRoot '.tools'
    $nugetDir = Join-Path $toolsDir 'nuget'
    $nugetExe = Join-Path $nugetDir 'nuget.exe'

    New-Item -ItemType Directory -Force -Path $nugetDir | Out-Null
    if (-not (Test-Path $nugetExe)) {
        Write-InstallLog '==> Downloading nuget.exe'
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri 'https://dist.nuget.org/win-x86-commandline/latest/nuget.exe' -OutFile $nugetExe
    }

    Write-InstallLog '==> Installing Microsoft.ArtifactSigning.Client from NuGet'
    & $nugetExe install Microsoft.ArtifactSigning.Client -OutputDirectory $toolsDir -NonInteractive
    if ($LASTEXITCODE -ne 0) {
        throw "nuget install Microsoft.ArtifactSigning.Client failed (exit $LASTEXITCODE)"
    }
}

function Install-ArtifactSigningToolsFromMsi {
    param([Parameter(Mandatory = $true)][string]$Url)

    $msiPath = Join-Path $env:TEMP 'ArtifactSigningClientTools.msi'
    Write-InstallLog "==> Installing Artifact Signing Client Tools via MSI ($Url)"
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest -Uri $Url -OutFile $msiPath
    $install = Start-Process msiexec.exe -Wait -PassThru -ArgumentList @(
        '/i', $msiPath, '/quiet', '/norestart'
    )
    Remove-Item $msiPath -Force -ErrorAction SilentlyContinue
    return $install.ExitCode
}

$existing = Find-ArtifactSigningDlib
if ($existing) {
    Write-InstallLog "==> Artifact Signing Client Tools already installed: $existing"
    Publish-ArtifactSigningDlibPath -Path $existing
    exit 0
}

if (Get-Command winget -ErrorAction SilentlyContinue) {
    Write-InstallLog '==> Installing Artifact Signing Client Tools via winget'
    winget install -e --id Microsoft.Azure.ArtifactSigningClientTools `
        --accept-package-agreements --accept-source-agreements
    if ($LASTEXITCODE -eq 0) {
        $installed = Find-ArtifactSigningDlib
        if ($installed) {
            Write-InstallLog "==> Installed: $installed"
            Publish-ArtifactSigningDlibPath -Path $installed
            exit 0
        }
    }
    Write-InstallLog "==> winget install did not complete successfully (exit $LASTEXITCODE); trying MSI"
}

$msiUrls = @(
    'https://download.microsoft.com/download/a3c24ba9-ff1f-444f-b626-eff710f345c3/ArtifactSigningClientTools.msi',
    'https://download.microsoft.com/download/70ad2c3b-761f-4aa9-a9de-e7405aa2b4c1/ArtifactSigningClientTools.msi'
)

foreach ($msiUrl in $msiUrls) {
    try {
        $exitCode = Install-ArtifactSigningToolsFromMsi -Url $msiUrl
        if ($exitCode -eq 0) {
            $installed = Find-ArtifactSigningDlib
            if ($installed) {
                Write-InstallLog "==> Installed: $installed"
                Publish-ArtifactSigningDlibPath -Path $installed
                exit 0
            }
        }
        Write-InstallLog "==> MSI install exit $exitCode from $msiUrl"
    } catch {
        Write-InstallLog "==> MSI download/install failed for $msiUrl : $_"
    }
}

try {
    Install-ArtifactSigningDlibFromNuGet
    $installed = Find-ArtifactSigningDlib
    if ($installed) {
        Write-InstallLog "==> Installed from NuGet: $installed"
        Publish-ArtifactSigningDlibPath -Path $installed
        exit 0
    }
} catch {
    Write-InstallLog "==> NuGet install failed: $_"
}

Write-Error @"
Artifact Signing Client Tools are not installed and automatic install failed.

Install once on the Windows build machine (Administrator PowerShell), then re-run CI:
  winget install -e --id Microsoft.Azure.ArtifactSigningClientTools

Default install location is usually:
  $env:LOCALAPPDATA\Microsoft\ArtifactSigningClientTools
"@
