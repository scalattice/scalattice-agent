# Resolve the recommended NVIDIA Game Ready (DCH) driver for this PC.
# Writes an INI file for the Scalattice Agent installer (Inno Setup).
param(
    [Parameter(Mandatory = $true)][string]$OutFile
)

$ErrorActionPreference = "Stop"

function Write-Result {
    param(
        [string]$GpuName = "",
        [string]$DeviceId = "",
        [string]$IsLaptop = "0",
        [string]$Version = "",
        [string]$DownloadUrl = "",
        [string]$DetailsUrl = "",
        [string]$ErrorMessage = ""
    )
    $dir = Split-Path -Parent $OutFile
    if ($dir -and -not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
    }
    @"
[Driver]
GpuName=$GpuName
DeviceId=$DeviceId
IsLaptop=$IsLaptop
Version=$Version
DownloadUrl=$DownloadUrl
DetailsUrl=$DetailsUrl
Error=$ErrorMessage
"@ | Set-Content -LiteralPath $OutFile -Encoding ASCII
}

function Get-NvidiaPciDevices {
    $devices = @()
    try {
        $controllers = Get-CimInstance -ClassName Win32_VideoController -ErrorAction SilentlyContinue
        foreach ($c in $controllers) {
            if ($c.PNPDeviceID -match 'VEN_10DE.+DEV_([0-9A-Fa-f]{4})') {
                $devices += [pscustomobject]@{
                    Name     = $c.Name
                    DeviceId = $Matches[1].ToUpperInvariant()
                }
            }
        }
    } catch {}

    if ($devices.Count -eq 0) {
        try {
            $entities = Get-CimInstance -ClassName Win32_PnPEntity -ErrorAction SilentlyContinue |
                Where-Object { $_.PNPDeviceID -match 'VEN_10DE' -and $_.PNPDeviceID -match 'DEV_' }
            foreach ($e in $entities) {
                if ($e.PNPDeviceID -match 'DEV_([0-9A-Fa-f]{4})') {
                    $dev = $Matches[1].ToUpperInvariant()
                    # Skip HD Audio / USB / unrelated NVIDIA PCI functions when possible.
                    $name = [string]$e.Name
                    if ($name -match '(?i)audio|usb|root|bridge') { continue }
                    $devices += [pscustomobject]@{ Name = $name; DeviceId = $dev }
                }
            }
        } catch {}
    }

    $devices | Sort-Object DeviceId -Unique
}

function Test-IsLaptop {
    try {
        $cs = Get-CimInstance -ClassName Win32_ComputerSystem -ErrorAction Stop
        if ([int]$cs.PCSystemType -eq 2) { return $true }
    } catch {}
    try {
        $enclosure = Get-CimInstance -ClassName Win32_SystemEnclosure -ErrorAction Stop
        $laptopChassis = @(8, 9, 10, 11, 12, 14, 18, 21, 30, 31, 32)
        foreach ($t in @($enclosure.ChassisTypes)) {
            if ($laptopChassis -contains [int]$t) { return $true }
        }
    } catch {}
    return $false
}

function Get-WindowsBuildInfo {
    $os = Get-CimInstance -ClassName Win32_OperatingSystem
    $ver = [version]$os.Version
    # GFE expects "10.0" for both Windows 10 and 11.
    return @{
        OsC   = "10.0"
        OsB   = [string]$ver.Build
    }
}

function Resolve-DriverFromGfe {
    param(
        [string]$DeviceId,
        [bool]$Laptop
    )

    $win = Get-WindowsBuildInfo
    $iLp = if ($Laptop) { "1" } else { "0" }
    # Build JSON by hand — Windows PowerShell 5 flattens single-element arrays in ConvertTo-Json.
    $json = (@'
{"dIDa":["DEVICE_10DE"],"osC":"OSC","osB":"OSB","is6":"1","lg":"1033","iLp":"ILP","prvMd":"0","gcV":"3.28.0.417","gIsB":"0","dch":"1","upCRD":"0","isCRD":"0"}
'@).Replace("DEVICE", $DeviceId).Replace("OSC", $win.OsC).Replace("OSB", $win.OsB).Replace("ILP", $iLp)

    $endpoint = "https://gfwsl.geforce.com/nvidia_web_services/controller.gfeclientcontent.NG.php/com.nvidia.services.GFEClientContent_NG.getDispDrvrByDevid/"
    $url = $endpoint + $json

    $resp = Invoke-WebRequest -Uri $url -UseBasicParsing -TimeoutSec 25 -Headers @{
        "User-Agent" = "NvBackend/36.0.0.0"
    }
    $data = $resp.Content | ConvertFrom-Json
    $attrs = $data.DriverAttributes
    if (-not $attrs) {
        throw "NVIDIA lookup returned no DriverAttributes"
    }

    $download = [string]$attrs.DownloadURLAdmin
    if (-not $download) { $download = [string]$attrs.DownloadURL }
    if (-not $download) { throw "NVIDIA lookup returned no download URL" }

    return [pscustomobject]@{
        Version     = [string]$attrs.Version
        DownloadUrl = $download
        DetailsUrl  = [string]$attrs.DetailsURL
    }
}

try {
    $gpus = @(Get-NvidiaPciDevices)
    if ($gpus.Count -eq 0) {
        Write-Result -ErrorMessage "No NVIDIA GPU found in Windows device list."
        exit 0
    }

    # Prefer a name that looks like a display GPU.
    $gpu = $gpus | Where-Object { $_.Name -match '(?i)geforce|rtx|gtx|quadro|tesla|rtx|nvidia' } | Select-Object -First 1
    if (-not $gpu) { $gpu = $gpus[0] }

    $laptop = Test-IsLaptop
    $driver = Resolve-DriverFromGfe -DeviceId $gpu.DeviceId -Laptop $laptop

    Write-Result `
        -GpuName $gpu.Name `
        -DeviceId $gpu.DeviceId `
        -IsLaptop ($(if ($laptop) { "1" } else { "0" })) `
        -Version $driver.Version `
        -DownloadUrl $driver.DownloadUrl `
        -DetailsUrl $driver.DetailsUrl
    exit 0
} catch {
    Write-Result -ErrorMessage ($_.Exception.Message -replace '[\r\n]+', ' ')
    exit 0
}
