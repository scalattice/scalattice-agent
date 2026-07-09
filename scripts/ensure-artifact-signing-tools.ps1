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

function Find-ArtifactSigningDlib {
    $fileName = 'Azure.CodeSigning.Dlib.dll'
    $searchRoots = @(
        "${env:ProgramFiles}\Microsoft\Azure Artifact Signing Client Tools",
        "${env:ProgramFiles}\Microsoft Artifact Signing Client Tools",
        "${env:ProgramFiles(x86)}\Microsoft\Azure Artifact Signing Client Tools"
    )

    foreach ($root in $searchRoots) {
        if (-not (Test-Path $root)) { continue }
        $match = Get-ChildItem $root -Recurse -Filter $fileName -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\x64\\' } |
            Select-Object -First 1
        if ($match) {
            return $match.FullName
        }
    }
    return $null
}

$existing = Find-ArtifactSigningDlib
if ($existing) {
    Write-InstallLog "==> Artifact Signing Client Tools already installed: $existing"
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
            exit 0
        }
    }
    Write-InstallLog "==> winget install did not complete successfully (exit $LASTEXITCODE); trying MSI"
}

$msiUrl = 'https://download.microsoft.com/download/70ad2c3b-761f-4aa9-a9de-e7405aa2f4c1/ArtifactSigningClientTools.msi'
$msiPath = Join-Path $env:TEMP 'ArtifactSigningClientTools.msi'

Write-InstallLog '==> Installing Artifact Signing Client Tools via MSI'
$ProgressPreference = 'SilentlyContinue'
Invoke-WebRequest -Uri $msiUrl -OutFile $msiPath
$install = Start-Process msiexec.exe -Wait -PassThru -ArgumentList @(
    '/i', $msiPath, '/quiet', '/norestart'
)
Remove-Item $msiPath -Force -ErrorAction SilentlyContinue

if ($install.ExitCode -ne 0) {
    Write-Error @"
Artifact Signing Client Tools install failed (msiexec exit $($install.ExitCode)).

Install once on the Windows build machine (Administrator PowerShell), then re-run CI:
  winget install -e --id Microsoft.Azure.ArtifactSigningClientTools

Or download ArtifactSigningClientTools.msi from Microsoft Learn and install manually.
"@
}

$installed = Find-ArtifactSigningDlib
if (-not $installed) {
    Write-Error 'Artifact Signing Client Tools MSI finished but Azure.CodeSigning.Dlib.dll was not found.'
}

Write-InstallLog "==> Installed: $installed"
