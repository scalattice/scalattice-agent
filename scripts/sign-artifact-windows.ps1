# Sign Windows release binaries with Azure Artifact Signing (Trusted Signing).
#
# Requires Artifact Signing Client Tools + Windows SDK SignTool on the build machine:
#   winget install -e --id Microsoft.Azure.ArtifactSigningClientTools
#
# Auth (service principal): set AZURE_CLIENT_SECRET locally or in CI secrets.
# Tenant and client ID default to the Scalattice signing app; override via env if needed.
#
# Usage:
#   . .\scripts\sign-artifact-windows.ps1
#   Invoke-ScalatticeArtifactSigning -Paths dist\scalattice-agent.exe

$ErrorActionPreference = 'Stop'

$Script:MinSignToolVersion = [version]'10.0.22621.755'
$Script:SigningTimestampUrl = 'http://timestamp.acs.microsoft.com'
$Script:DefaultSigningEndpoint = 'https://neu.codesigning.azure.net'
$Script:DefaultSigningAccount = 'scalattice'
$Script:DefaultSigningProfile = 'Scalattice'
$Script:DefaultAzureTenantId = '5d902829-56a2-41e5-812d-a3ec3a502f3d'
$Script:DefaultAzureClientId = '0c6c6d06-09c9-4da3-a72c-516eecde1b5e'

function Get-ScalatticeSigningConfig {
    @{
        Endpoint = if ($env:SCALATTICE_SIGNING_ENDPOINT) { $env:SCALATTICE_SIGNING_ENDPOINT.TrimEnd('/') } else { $Script:DefaultSigningEndpoint }
        CodeSigningAccountName = if ($env:SCALATTICE_SIGNING_ACCOUNT) { $env:SCALATTICE_SIGNING_ACCOUNT } else { $Script:DefaultSigningAccount }
        CertificateProfileName = if ($env:SCALATTICE_SIGNING_PROFILE) { $env:SCALATTICE_SIGNING_PROFILE } else { $Script:DefaultSigningProfile }
        TenantId = if ($env:AZURE_TENANT_ID) { $env:AZURE_TENANT_ID } else { $Script:DefaultAzureTenantId }
        ClientId = if ($env:AZURE_CLIENT_ID) { $env:AZURE_CLIENT_ID } else { $Script:DefaultAzureClientId }
        ClientSecret = $env:AZURE_CLIENT_SECRET
    }
}

function Test-ScalatticeSigningEnabled {
    $config = Get-ScalatticeSigningConfig
    return [bool]$config.ClientSecret
}

function Find-SignToolExe {
    if ($env:SCALATTICE_SIGNTOOL_PATH -and (Test-Path $env:SCALATTICE_SIGNTOOL_PATH)) {
        return (Resolve-Path $env:SCALATTICE_SIGNTOOL_PATH).Path
    }

    $candidates = @()
    $kitsRoot = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
    if (Test-Path $kitsRoot) {
        Get-ChildItem $kitsRoot -Directory -Filter '10.0.*' | ForEach-Object {
            $tool = Join-Path $_.FullName 'x64\signtool.exe'
            if (Test-Path $tool) {
                $candidates += [pscustomobject]@{
                    Path = $tool
                    Version = [version]($_.Name)
                }
            }
        }
    }

    $repoRoot = Split-Path $PSScriptRoot -Parent
    $nugetRoot = Join-Path $repoRoot '.tools\Microsoft.Windows.SDK.BuildTools'
    if (Test-Path $nugetRoot) {
        Get-ChildItem $nugetRoot -Recurse -Filter 'signtool.exe' -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\x64\\signtool\.exe$' } |
            ForEach-Object {
                $candidates += [pscustomobject]@{
                    Path = $_.FullName
                    Version = $Script:MinSignToolVersion
                }
            }
    }

    $best = $candidates |
        Where-Object { $_.Version -ge $Script:MinSignToolVersion } |
        Sort-Object Version -Descending |
        Select-Object -First 1

    if ($best) {
        return $best.Path
    }
    return $null
}

function Find-ArtifactSigningDlib {
    if ($env:SCALATTICE_SIGN_DLIB -and (Test-Path $env:SCALATTICE_SIGN_DLIB)) {
        return (Resolve-Path $env:SCALATTICE_SIGN_DLIB).Path
    }

    $fileName = 'Azure.CodeSigning.Dlib.dll'
    $searchRoots = @(
        "${env:ProgramFiles}\Microsoft\Azure Artifact Signing Client Tools",
        "${env:ProgramFiles}\Microsoft Artifact Signing Client Tools",
        "${env:ProgramFiles(x86)}\Microsoft\Azure Artifact Signing Client Tools",
        (Join-Path $PSScriptRoot '..\.tools\Microsoft.ArtifactSigning.Client')
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

function New-ScalatticeSigningMetadataFile {
    param(
        [Parameter(Mandatory = $true)][hashtable]$Config,
        [Parameter(Mandatory = $true)][string]$OutPath
    )

    $metadata = [ordered]@{
        Endpoint = $Config.Endpoint
        CodeSigningAccountName = $Config.CodeSigningAccountName
        CertificateProfileName = $Config.CertificateProfileName
        ExcludeCredentials = @(
            'ManagedIdentityCredential',
            'WorkloadIdentityCredential',
            'SharedTokenCacheCredential',
            'VisualStudioCredential',
            'VisualStudioCodeCredential',
            'AzureCliCredential',
            'AzurePowerShellCredential',
            'AzureDeveloperCliCredential',
            'InteractiveBrowserCredential'
        )
    }

    $json = ($metadata | ConvertTo-Json -Depth 4)
    $dir = Split-Path $OutPath -Parent
    if ($dir -and -not (Test-Path $dir)) {
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
    }
    Set-Content -LiteralPath $OutPath -Value $json -Encoding utf8
    return $OutPath
}

function Invoke-ScalatticeArtifactSigning {
    param(
        [Parameter(Mandatory = $true)][string[]]$Paths,
        [switch]$Required
    )

    $config = Get-ScalatticeSigningConfig
    if (-not $config.ClientSecret) {
        if ($Required) {
            Write-Error @"
Azure Artifact Signing requires AZURE_CLIENT_SECRET.

Create a client secret for app $($config.ClientId), assign Trusted Signing Certificate Profile Signer
on profile $($config.CertificateProfileName), then set:

  `$env:AZURE_CLIENT_SECRET = '<secret>'
"@
        }
        Write-Host "==> Skipping code signing (AZURE_CLIENT_SECRET not set)"
        return
    }

    if (-not $env:AZURE_TENANT_ID) { $env:AZURE_TENANT_ID = $config.TenantId }
    if (-not $env:AZURE_CLIENT_ID) { $env:AZURE_CLIENT_ID = $config.ClientId }

    $files = @(
        $Paths |
            ForEach-Object { Resolve-Path $_ -ErrorAction SilentlyContinue } |
            ForEach-Object { $_.Path }
    )
    if ($files.Count -eq 0) {
        if ($Required) {
            Write-Error "No files to sign."
        }
        Write-Warning "No signing targets found."
        return
    }

    $signtool = Find-SignToolExe
    if (-not $signtool) {
        Write-Error @"
SignTool.exe not found (need Windows SDK >= $($Script:MinSignToolVersion)).

Install Visual Studio Build Tools with Windows SDK, or set SCALATTICE_SIGNTOOL_PATH.
"@
    }

    $dlib = Find-ArtifactSigningDlib
    if (-not $dlib) {
        Write-Error @"
Azure Artifact Signing dlib not found.

Install client tools:
  winget install -e --id Microsoft.Azure.ArtifactSigningClientTools

Or set SCALATTICE_SIGN_DLIB to Azure.CodeSigning.Dlib.dll (x64).
"@
    }

    $metadataPath = Join-Path ([System.IO.Path]::GetTempPath()) "scalattice-artifact-signing-metadata.json"
    New-ScalatticeSigningMetadataFile -Config $config -OutPath $metadataPath | Out-Null

    Write-Host "==> Azure Artifact Signing"
    Write-Host "    endpoint: $($config.Endpoint)"
    Write-Host "    account:  $($config.CodeSigningAccountName)"
    Write-Host "    profile:  $($config.CertificateProfileName)"
    Write-Host "    signtool: $signtool"

    foreach ($file in $files) {
        Write-Host "==> Signing $(Split-Path $file -Leaf)"
        & $signtool sign /v /fd SHA256 /tr $Script:SigningTimestampUrl /td SHA256 /dlib $dlib /dmdf $metadataPath $file
        if ($LASTEXITCODE -ne 0) {
            Write-Error "signtool failed for $file (exit $LASTEXITCODE)"
        }
        & $signtool verify /pa /v $file
        if ($LASTEXITCODE -ne 0) {
            Write-Error "Signature verification failed for $file"
        }
        Write-Host "    verified"
    }
}
