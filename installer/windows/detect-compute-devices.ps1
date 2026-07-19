# Inventory CPU + GPUs for the Scalattice Agent installer (Inno Setup).
# Uses only built-in Windows APIs — no nvidia-smi / CUDA / vendor SDKs required.
param(
    [Parameter(Mandatory = $true)][string]$OutFile
)

$ErrorActionPreference = "SilentlyContinue"

function Get-CpuName {
    try {
        $cpu = Get-CimInstance -ClassName Win32_Processor -ErrorAction Stop | Select-Object -First 1
        if ($cpu -and $cpu.Name) { return ([string]$cpu.Name).Trim() }
    } catch {}
    return "CPU (unknown)"
}

function Get-SystemRamMb {
    try {
        $cs = Get-CimInstance -ClassName Win32_ComputerSystem -ErrorAction Stop | Select-Object -First 1
        if ($cs -and $null -ne $cs.TotalPhysicalMemory) {
            $n = [uint64]$cs.TotalPhysicalMemory
            if ($n -gt 0) {
                return [int]([math]::Round($n / 1MB))
            }
        }
    } catch {}
    # Fallback: sum DIMM capacities when TotalPhysicalMemory is unavailable.
    try {
        $total = [uint64]0
        foreach ($stick in @(Get-CimInstance -ClassName Win32_PhysicalMemory -ErrorAction SilentlyContinue)) {
            if ($null -ne $stick.Capacity) {
                $total += [uint64]$stick.Capacity
            }
        }
        if ($total -gt 0) {
            return [int]([math]::Round($total / 1MB))
        }
    } catch {}
    return 0
}

function Test-IsIntegratedName([string]$name) {
    $lower = $name.ToLowerInvariant()
    if ($lower -match 'nvidia|geforce|quadro|rtx |gtx ') { return $false }
    return [bool]($lower -match 'intel|uhd|iris|hd graphics|radeon graphics|vega|mali|amd radeon\(tm\) graphics')
}

function Test-NvidiaDriverPresent {
    $sys = Join-Path $env:WINDIR "System32\nvcuda.dll"
    $wow = Join-Path $env:WINDIR "SysWOW64\nvcuda.dll"
    return (Test-Path -LiteralPath $sys) -or (Test-Path -LiteralPath $wow)
}

function Convert-ToVramMb($bytes) {
    if ($null -eq $bytes) { return 0 }
    try {
        $n = [uint64]$bytes
        if ($n -le 0) { return 0 }
        return [int]([math]::Round($n / 1MB))
    } catch {
        return 0
    }
}

# Display-adapter class: HardwareInformation.qwMemorySize is a real QWORD
# (Win32_VideoController.AdapterRAM is a broken 32-bit field and lies above ~2-4 GB).
function Get-RegistryVramMap {
    $map = @{}
    $root = "HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}"
    try {
        Get-ChildItem -LiteralPath $root -ErrorAction SilentlyContinue | ForEach-Object {
            if ($_.PSChildName -notmatch '^\d{4}$') { return }
            try {
                $props = Get-ItemProperty -LiteralPath $_.PSPath -ErrorAction SilentlyContinue
                $desc = [string]$props.DriverDesc
                if (-not $desc -or -not $desc.Trim()) { return }

                $mb = 0
                if ($null -ne $props.'HardwareInformation.qwMemorySize') {
                    $mb = Convert-ToVramMb $props.'HardwareInformation.qwMemorySize'
                }
                # DWORD MemorySize only trusted below 2 GiB (same overflow class as AdapterRAM).
                if ($mb -le 0 -and $null -ne $props.'HardwareInformation.MemorySize') {
                    $dw = [uint64]([uint32]$props.'HardwareInformation.MemorySize')
                    if ($dw -gt 0 -and $dw -lt 2GB) { $mb = Convert-ToVramMb $dw }
                }
                if ($mb -gt 0) {
                    $map[$desc.Trim().ToLowerInvariant()] = $mb
                }
            } catch {}
        }
    } catch {}
    return $map
}

# dxdiag is built into Windows; use only when registry did not cover every adapter.
function Get-DxdiagVramMap {
    $map = @{}
    $xmlPath = Join-Path $env:TEMP ("scalattice-dxdiag-{0}.xml" -f [guid]::NewGuid().ToString("N"))
    try {
        $dxdiag = Join-Path $env:WINDIR "System32\dxdiag.exe"
        if (-not (Test-Path -LiteralPath $dxdiag)) { return $map }
        $p = Start-Process -FilePath $dxdiag -ArgumentList @("/x", $xmlPath) -WindowStyle Hidden -Wait -PassThru
        if (-not (Test-Path -LiteralPath $xmlPath)) { return $map }
        [xml]$xml = Get-Content -LiteralPath $xmlPath -Raw -ErrorAction Stop
        $devices = @($xml.SelectNodes("//DisplayDevice"))
        foreach ($d in $devices) {
            $name = [string]$d.CardName
            if (-not $name) { $name = [string]$d.ChipType }
            if (-not $name) { continue }
            $memText = [string]$d.DedicatedMemory
            if (-not $memText) { $memText = [string]$d.DisplayMemory }
            if (-not $memText) { continue }
            $mb = 0
            if ($memText -match '(?i)([\d\.]+)\s*GB') {
                $mb = [int]([math]::Round([double]$Matches[1] * 1024))
            } elseif ($memText -match '(?i)([\d\.]+)\s*MB') {
                $mb = [int]([math]::Round([double]$Matches[1]))
            }
            if ($mb -gt 0) {
                $map[$name.Trim().ToLowerInvariant()] = $mb
            }
        }
    } catch {
    } finally {
        Remove-Item -LiteralPath $xmlPath -Force -ErrorAction SilentlyContinue
    }
    return $map
}

function Resolve-VramMb([string]$name, $maps) {
    $key = $name.ToLowerInvariant()
    foreach ($map in $maps) {
        if ($map.ContainsKey($key)) { return [int]$map[$key] }
    }
    foreach ($map in $maps) {
        foreach ($k in @($map.Keys)) {
            if ($key.Contains($k) -or $k.Contains($key)) { return [int]$map[$k] }
        }
    }
    return 0
}

function Get-VideoControllers {
    $regMap = Get-RegistryVramMap
    $list = @()

    try {
        $controllers = Get-CimInstance -ClassName Win32_VideoController -ErrorAction SilentlyContinue
        foreach ($c in @($controllers)) {
            $name = [string]$c.Name
            if (-not $name -or -not $name.Trim()) { continue }
            if ($name -match '(?i)microsoft basic|remote desktop|virtualbox|vmware svga|hyper-v') { continue }

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

            $driverOk = $true
            try {
                if ($null -ne $c.ConfigManagerErrorCode -and [int]$c.ConfigManagerErrorCode -ne 0) {
                    $driverOk = $false
                }
            } catch {}

            $driverVer = ""
            try {
                if ($c.DriverVersion) { $driverVer = ([string]$c.DriverVersion).Trim() }
            } catch {}

            $list += [pscustomobject]@{
                Name          = $name.Trim()
                Kind          = $kind
                Vendor        = $vendor
                VramMb        = 0
                DriverOk      = $driverOk
                DriverVersion = $driverVer
                # Stable unique id — identical card names (e.g. 2x T400) must not collapse.
                InstanceId    = if ($pnp) { $pnp.Trim() } else { "$($name.Trim())|$($list.Count)" }
            }
        }
    } catch {}

    $seen = @{}
    $unique = @()
    foreach ($g in $list) {
        $key = if ($g.InstanceId) {
            $g.InstanceId.ToLowerInvariant()
        } else {
            $g.Name.ToLowerInvariant()
        }
        if ($seen.ContainsKey($key)) { continue }
        $seen[$key] = $true
        $unique += $g
    }

    $maps = @($regMap)
    $needDxdiag = $false
    foreach ($g in $unique) {
        $g.VramMb = Resolve-VramMb -name $g.Name -maps $maps
        if ($g.Vendor -ne "intel" -and $g.Kind -eq "discrete" -and $g.VramMb -le 0) {
            $needDxdiag = $true
        }
    }
    if ($needDxdiag) {
        $dxMap = Get-DxdiagVramMap
        if ($dxMap.Count -gt 0) {
            $maps = @($dxMap, $regMap)
            foreach ($g in $unique) {
                if ($g.VramMb -le 0) {
                    $g.VramMb = Resolve-VramMb -name $g.Name -maps $maps
                }
            }
        }
    }

    return $unique
}

$dir = Split-Path -Parent $OutFile
if ($dir -and -not (Test-Path -LiteralPath $dir)) {
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
}

$cpu = Get-CpuName
$ramMb = Get-SystemRamMb
$gpus = @(Get-VideoControllers)
$nvidiaGpus = @($gpus | Where-Object { $_.Vendor -eq "nvidia" })
$nvidiaPresent = $nvidiaGpus.Count -gt 0
$nvidiaDeviceOk = $nvidiaPresent -and (@($nvidiaGpus | Where-Object { $_.DriverOk }).Count -gt 0)
# Usable NVIDIA stack without calling nvidia-smi: healthy WDDM device + nvcuda.dll on disk.
$nvidiaReady = $nvidiaDeviceOk -and (Test-NvidiaDriverPresent)
$driverVer = ""
if ($nvidiaPresent) {
    $driverVer = [string](@($nvidiaGpus | Where-Object { $_.DriverVersion } | Select-Object -First 1).DriverVersion)
}

$sb = New-Object System.Text.StringBuilder
[void]$sb.AppendLine("[Inventory]")
[void]$sb.AppendLine("CpuName=$cpu")
[void]$sb.AppendLine("RamMb=$ramMb")
[void]$sb.AppendLine("GpuCount=$($gpus.Count)")
[void]$sb.AppendLine("NvidiaPresent=$(if ($nvidiaPresent) { '1' } else { '0' })")
[void]$sb.AppendLine("NvidiaSmiOk=$(if ($nvidiaReady) { '1' } else { '0' })")
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
