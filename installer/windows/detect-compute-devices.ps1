# Inventory CPU + GPUs for the Scalattice Agent installer (Inno Setup).
# Writes an INI file the wizard reads on the Compatible devices page.
param(
    [Parameter(Mandatory = $true)][string]$OutFile
)

$ErrorActionPreference = "SilentlyContinue"

function Get-NvidiaSmiPath {
    $candidates = @(
        (Join-Path $env:WINDIR "System32\nvidia-smi.exe"),
        (Join-Path ${env:ProgramFiles} "NVIDIA Corporation\NVSMI\nvidia-smi.exe"),
        (Join-Path ${env:ProgramFiles(x86)} "NVIDIA Corporation\NVSMI\nvidia-smi.exe"),
        "nvidia-smi.exe"
    )
    foreach ($c in $candidates) {
        if ($c -eq "nvidia-smi.exe") { return $c }
        if (Test-Path -LiteralPath $c) { return $c }
    }
    return $null
}

function Test-NvidiaSmiOk {
    $smi = Get-NvidiaSmiPath
    if (-not $smi) { return $false }
    try {
        $p = Start-Process -FilePath $smi -ArgumentList @("-L") -WindowStyle Hidden -Wait -PassThru
        return ($p.ExitCode -eq 0)
    } catch {
        return $false
    }
}

function Get-NvidiaDriverVersion {
    $smi = Get-NvidiaSmiPath
    if (-not $smi) { return "" }
    try {
        $out = & $smi --query-gpu=driver_version --format=csv,noheader 2>$null
        if ($LASTEXITCODE -ne 0) { return "" }
        $line = (@($out) | Where-Object { $_ -and $_.Trim() } | Select-Object -First 1)
        if ($line) { return $line.Trim() }
    } catch {}
    return ""
}

function Get-CpuName {
    try {
        $cpu = Get-CimInstance -ClassName Win32_Processor -ErrorAction Stop | Select-Object -First 1
        if ($cpu -and $cpu.Name) { return ([string]$cpu.Name).Trim() }
    } catch {}
    return "CPU (unknown)"
}

function Test-IsIntegratedName([string]$name) {
    $lower = $name.ToLowerInvariant()
    if ($lower -match 'nvidia|geforce|quadro|rtx |gtx ') { return $false }
    return [bool]($lower -match 'intel|uhd|iris|hd graphics|radeon graphics|vega|mali|amd radeon\(tm\) graphics')
}

function Get-VideoControllers {
    $list = @()
    try {
        $controllers = Get-CimInstance -ClassName Win32_VideoController -ErrorAction SilentlyContinue
        foreach ($c in @($controllers)) {
            $name = [string]$c.Name
            if (-not $name -or -not $name.Trim()) { continue }
            $pnp = [string]$c.PNPDeviceID
            $vendor = "other"
            $kind = "discrete"
            if ($pnp -match 'VEN_10DE' -or $name -match '(?i)nvidia|geforce|quadro|rtx |gtx ') {
                $vendor = "nvidia"
                $kind = "discrete"
            } elseif ($pnp -match 'VEN_1002' -or $name -match '(?i)amd|radeon') {
                $vendor = "amd"
                $kind = if (Test-IsIntegratedName $name) { "integrated" } else { "discrete" }
            } elseif ($pnp -match 'VEN_8086' -or $name -match '(?i)intel') {
                $vendor = "intel"
                $kind = if (Test-IsIntegratedName $name) { "integrated" } else { "discrete" }
            } elseif (Test-IsIntegratedName $name) {
                $kind = "integrated"
            }
            $vramMb = 0
            try {
                if ($c.AdapterRAM -and [int64]$c.AdapterRAM -gt 0 -and [int64]$c.AdapterRAM -lt [int64]([uint32]::MaxValue)) {
                    $vramMb = [int]([math]::Round([int64]$c.AdapterRAM / 1MB))
                }
            } catch {}
            $list += [pscustomobject]@{
                Name   = $name.Trim()
                Kind   = $kind
                Vendor = $vendor
                VramMb = $vramMb
                PnpId  = $pnp
            }
        }
    } catch {}

    # Deduplicate by name (case-insensitive).
    $seen = @{}
    $unique = @()
    foreach ($g in $list) {
        $key = $g.Name.ToLowerInvariant()
        if ($seen.ContainsKey($key)) { continue }
        $seen[$key] = $true
        $unique += $g
    }
    return $unique
}

$dir = Split-Path -Parent $OutFile
if ($dir -and -not (Test-Path -LiteralPath $dir)) {
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
}

$cpu = Get-CpuName
$gpus = @(Get-VideoControllers)
$nvidiaPresent = @($gpus | Where-Object { $_.Vendor -eq "nvidia" }).Count -gt 0
$smiOk = Test-NvidiaSmiOk
$driverVer = ""
if ($smiOk) { $driverVer = Get-NvidiaDriverVersion }

$sb = New-Object System.Text.StringBuilder
[void]$sb.AppendLine("[Inventory]")
[void]$sb.AppendLine("CpuName=$cpu")
[void]$sb.AppendLine("GpuCount=$($gpus.Count)")
[void]$sb.AppendLine("NvidiaPresent=$(if ($nvidiaPresent) { '1' } else { '0' })")
[void]$sb.AppendLine("NvidiaSmiOk=$(if ($smiOk) { '1' } else { '0' })")
[void]$sb.AppendLine("NvidiaDriverVersion=$driverVer")

for ($i = 0; $i -lt $gpus.Count; $i++) {
    $g = $gpus[$i]
    [void]$sb.AppendLine("")
    [void]$sb.AppendLine("[Gpu$i]")
    [void]$sb.AppendLine("Name=$($g.Name)")
    [void]$sb.AppendLine("Kind=$($g.Kind)")
    [void]$sb.AppendLine("Vendor=$($g.Vendor)")
    [void]$sb.AppendLine("VramMb=$($g.VramMb)")
}

$sb.ToString() | Set-Content -LiteralPath $OutFile -Encoding ASCII
exit 0
