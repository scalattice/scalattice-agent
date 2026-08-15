use std::collections::HashSet;
use serde::{Deserialize, Serialize};
use std::process::Command;

/// On Windows, console tools (`nvidia-smi`, `powershell`, `where`) briefly flash a
/// cmd window unless CREATE_NO_WINDOW is set. Specs are refreshed on every
/// heartbeat, so that flash looks like an endless open/close loop.
fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[cfg(windows)]
fn powershell_hidden(args: &[&str]) -> Command {
    let mut cmd = Command::new("powershell");
    hide_console(&mut cmd);
    cmd.arg("-NoProfile");
    cmd.arg("-WindowStyle");
    cmd.arg("Hidden");
    for arg in args {
        cmd.arg(arg);
    }
    cmd
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeDevice {
    pub id: String,
    pub kind: String,
    pub name: String,
    #[serde(rename = "vramGb", skip_serializing_if = "Option::is_none")]
    pub vram_gb: Option<u32>,
    #[serde(rename = "vramUsedGb", skip_serializing_if = "Option::is_none")]
    pub vram_used_gb: Option<u32>,
    #[serde(rename = "utilPct", skip_serializing_if = "Option::is_none")]
    pub util_pct: Option<u8>,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MachineSpecs {
    #[serde(rename = "agentVersion", skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    #[serde(rename = "gpuName", skip_serializing_if = "Option::is_none")]
    pub gpu_name: Option<String>,
    #[serde(rename = "vramGb", skip_serializing_if = "Option::is_none")]
    pub vram_gb: Option<u32>,
    #[serde(rename = "vramUsedGb", skip_serializing_if = "Option::is_none")]
    pub vram_used_gb: Option<u32>,
    #[serde(rename = "gpuUtilPct", skip_serializing_if = "Option::is_none")]
    pub gpu_util_pct: Option<u8>,
    #[serde(rename = "gpuCount", skip_serializing_if = "Option::is_none")]
    pub gpu_count: Option<u8>,
    #[serde(rename = "driverVersion", skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
    #[serde(rename = "cudaVersion", skip_serializing_if = "Option::is_none")]
    pub cuda_version: Option<String>,
    #[serde(rename = "hostname", skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(rename = "cpuModel", skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    #[serde(rename = "ramGb", skip_serializing_if = "Option::is_none")]
    pub ram_gb: Option<u32>,
    #[serde(rename = "ramUsedGb", skip_serializing_if = "Option::is_none")]
    pub ram_used_gb: Option<u32>,
    #[serde(rename = "diskTotalGb", skip_serializing_if = "Option::is_none")]
    pub disk_total_gb: Option<u32>,
    #[serde(rename = "diskUsedGb", skip_serializing_if = "Option::is_none")]
    pub disk_used_gb: Option<u32>,
    #[serde(rename = "diskFreeGb", skip_serializing_if = "Option::is_none")]
    pub disk_free_gb: Option<u32>,
    #[serde(rename = "computeDevices", skip_serializing_if = "Vec::is_empty")]
    pub compute_devices: Vec<ComputeDevice>,
}

fn agent_version_string() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

pub fn detect_all_compute_devices() -> Vec<ComputeDevice> {
    #[cfg(target_os = "macos")]
    {
        let mut devices = Vec::new();
        devices.extend(detect_apple_metal_device());
        devices.extend(detect_cpu_device());
        for device in &mut devices {
            if device.kind == "metal" {
                device.enabled = true;
            }
        }
        return devices;
    }

    #[cfg(not(target_os = "macos"))]
    {
        let mut devices = Vec::new();
        devices.extend(detect_nvidia_devices());
        devices.extend(detect_amd_devices());
        devices.extend(detect_pci_vulkan_discrete_devices(&devices));
        devices.extend(detect_integrated_pci_devices(&devices));
        devices.extend(detect_cpu_device());

        for device in &mut devices {
            if device.kind == "discrete" {
                device.enabled = true;
            }
        }

        return devices;
    }

    #[allow(unreachable_code)]
    Vec::new()
}

pub fn apply_compute_policy(devices: &mut [ComputeDevice], policy: &[(String, bool)]) {
    if policy.is_empty() {
        return;
    }
    let policy_map: std::collections::HashMap<_, _> = policy.iter().cloned().collect();
    for device in devices {
        if let Some(enabled) = policy_map.get(&device.id) {
            device.enabled = *enabled;
        }
    }
}

pub fn detect_machine_specs() -> MachineSpecs {
    let hostname = detect_hostname();
    let cpu_model = detect_cpu_model();
    let ram_gb = detect_ram_gb();
    let devices = detect_all_compute_devices();
    if devices.is_empty() {
        let disk = disk_usage_gb().unwrap_or((None, None, None));
        return MachineSpecs {
            agent_version: Some(agent_version_string()),
            hostname,
            cpu_model,
            ram_gb,
            ram_used_gb: detect_ram_used_gb(),
            disk_total_gb: disk.0,
            disk_used_gb: disk.1,
            disk_free_gb: disk.2,
            ..MachineSpecs::default()
        };
    }

    build_specs_from_devices(&devices, hostname, cpu_model, ram_gb, None, None)
}

pub fn build_specs_from_devices(
    devices: &[ComputeDevice],
    hostname: Option<String>,
    cpu_model: Option<String>,
    ram_gb: Option<u32>,
    driver_version: Option<String>,
    cuda_version: Option<String>,
) -> MachineSpecs {
    let enabled: Vec<&ComputeDevice> = devices.iter().filter(|d| d.enabled).collect();
    let discrete_count = enabled
        .iter()
        .filter(|d| d.kind == "discrete")
        .count();

    let gpu_name = if enabled.len() == 1 {
        Some(enabled[0].name.clone())
    } else if enabled.len() > 1 {
        Some(
            enabled
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(" + "),
        )
    } else {
        None
    };

    let vram_gb = sum_option(
        enabled
            .iter()
            .filter(|d| d.kind == "discrete" || d.kind == "integrated")
            .filter_map(|d| d.vram_gb),
    );
    let vram_used_gb = sum_option(enabled.iter().filter_map(|d| d.vram_used_gb));
    let gpu_util_pct = enabled
        .iter()
        .filter_map(|d| d.util_pct)
        .max();

    let gpu_count = if discrete_count > 0 {
        Some(discrete_count.min(255) as u8)
    } else if !enabled.is_empty() {
        Some(enabled.len().min(255) as u8)
    } else {
        None
    };

    let disk = disk_usage_gb().unwrap_or((None, None, None));

    MachineSpecs {
        agent_version: Some(agent_version_string()),
        gpu_name,
        vram_gb,
        vram_used_gb,
        gpu_util_pct,
        gpu_count,
        driver_version,
        cuda_version,
        hostname,
        cpu_model,
        ram_gb,
        ram_used_gb: detect_ram_used_gb(),
        disk_total_gb: disk.0,
        disk_used_gb: disk.1,
        disk_free_gb: disk.2,
        compute_devices: devices.to_vec(),
    }
}

pub fn status_line(specs: &MachineSpecs) -> String {
    let enabled: Vec<_> = specs
        .compute_devices
        .iter()
        .filter(|d| d.enabled)
        .collect();

    if !enabled.is_empty() {
        let names: Vec<_> = enabled.iter().map(|d| d.name.as_str()).collect();
        let label = if names.len() > 1 {
            format!("{} devices enabled", names.len())
        } else {
            names[0].to_string()
        };
        let vram = specs
            .vram_gb
            .map(|vram| format!(" · {vram} GB VRAM"))
            .unwrap_or_default();
        let util = specs
            .gpu_util_pct
            .map(|pct| format!(" · {pct}% load"))
            .unwrap_or_default();
        return format!("Compute enabled · {label}{vram}{util}");
    }

    match (&specs.gpu_name, specs.vram_gb) {
        (Some(name), Some(vram)) => {
            let util = specs
                .gpu_util_pct
                .map(|pct| format!(" · {pct}% load"))
                .unwrap_or_default();
            format!("GPU detected · {name} · {vram} GB VRAM{util}")
        }
        (Some(name), None) => format!("GPU detected · {name}"),
        (None, _) => {
            let total = specs.compute_devices.len();
            let enabled = specs.compute_devices.iter().filter(|d| d.enabled).count();
            if total > 0 && enabled > 0 {
                format!("{enabled} of {total} compute device(s) enabled")
            } else if total > 0 {
                format!("{total} compute device(s) detected (none enabled in dashboard)")
            } else {
                "No GPU detected (install vendor tools or check drivers)".to_string()
            }
        }
    }
}

fn sum_option(values: impl Iterator<Item = u32>) -> Option<u32> {
    let total: u32 = values.sum();
    if total > 0 { Some(total) } else { None }
}

fn detect_nvidia_devices() -> Vec<ComputeDevice> {
    for bin in nvidia_smi_bins() {
        let devices = detect_nvidia_devices_from(&bin);
        if !devices.is_empty() {
            return devices;
        }
    }
    detect_nvidia_devices_from_procfs()
}

/// Absolute `nvidia-smi` paths first (Windows PATH is often incomplete for tray agents).
fn nvidia_smi_bins() -> Vec<String> {
    #[cfg(windows)]
    {
        let mut bins = Vec::new();
        let system32 = std::env::var_os("SystemRoot")
            .map(|root| {
                std::path::PathBuf::from(root)
                    .join("System32")
                    .join("nvidia-smi.exe")
            })
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows\System32\nvidia-smi.exe"));
        bins.push(system32.to_string_lossy().into_owned());

        if let Some(pf) = std::env::var_os("ProgramFiles") {
            bins.push(
                std::path::PathBuf::from(pf)
                    .join("NVIDIA Corporation")
                    .join("NVSMI")
                    .join("nvidia-smi.exe")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        if let Some(pf86) = std::env::var_os("ProgramFiles(x86)") {
            bins.push(
                std::path::PathBuf::from(pf86)
                    .join("NVIDIA Corporation")
                    .join("NVSMI")
                    .join("nvidia-smi.exe")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        bins.push("nvidia-smi".into());
        bins.push("nvidia-smi.exe".into());
        bins
    }
    #[cfg(unix)]
    {
        vec![
            "/usr/lib/wsl/lib/nvidia-smi".into(),
            "/usr/lib/nvidia/bin/nvidia-smi".into(),
            "/usr/bin/nvidia-smi".into(),
            "/usr/sbin/nvidia-smi".into(),
            "/usr/local/bin/nvidia-smi".into(),
            "/usr/local/cuda/bin/nvidia-smi".into(),
            "nvidia-smi".into(),
        ]
    }
    #[cfg(not(any(unix, windows)))]
    {
        vec!["nvidia-smi".into()]
    }
}

/// First working `nvidia-smi` for diagnostics (absolute path preferred).
#[allow(dead_code)]
pub fn resolve_nvidia_smi() -> Option<String> {
    for bin in nvidia_smi_bins() {
        if bin != "nvidia-smi"
            && bin != "nvidia-smi.exe"
            && !std::path::Path::new(&bin).is_file()
        {
            continue;
        }
        let Ok(output) = configure_nvidia_smi_command(&bin)
            .args(["-L"])
            .output()
        else {
            continue;
        };
        if output.status.success() {
            return Some(bin);
        }
    }
    None
}

fn wsl_nvidia_lib_dir() -> Option<&'static str> {
    if std::path::Path::new("/usr/lib/wsl/lib").is_dir() {
        Some("/usr/lib/wsl/lib")
    } else {
        None
    }
}

fn configure_nvidia_smi_command(bin: &str) -> Command {
    let mut cmd = Command::new(bin);
    hide_console(&mut cmd);
    if let Some(wsl_lib) = wsl_nvidia_lib_dir() {
        let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
        let path = if existing.is_empty() {
            wsl_lib.to_string()
        } else if existing.split(':').any(|part| part == wsl_lib) {
            existing
        } else {
            format!("{wsl_lib}:{existing}")
        };
        cmd.env("LD_LIBRARY_PATH", path);
    }
    cmd
}

fn detect_nvidia_devices_from_procfs() -> Vec<ComputeDevice> {
    #[cfg(not(unix))]
    {
        return Vec::new();
    }
    #[cfg(unix)]
    {
        let root = std::path::Path::new("/proc/driver/nvidia/gpus");
        let Ok(entries) = std::fs::read_dir(root) else {
            return Vec::new();
        };

        let mut devices = Vec::new();
        for (index, entry) in entries.flatten().enumerate() {
            let info_path = entry.path().join("information");
            let Ok(raw) = std::fs::read_to_string(info_path) else {
                continue;
            };
            let mut name = None;
            for line in raw.lines() {
                if let Some(rest) = line.strip_prefix("Model:") {
                    let trimmed = rest.trim();
                    if !trimmed.is_empty() {
                        name = Some(trimmed.to_string());
                    }
                }
            }
            let Some(name) = name else { continue };
            devices.push(ComputeDevice {
                id: format!("nvidia:{index}"),
                kind: "discrete".to_string(),
                name,
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            });
        }

        devices
    }
}

fn detect_nvidia_devices_from(bin: &str) -> Vec<ComputeDevice> {
    let Ok(output) = configure_nvidia_smi_command(bin)
        .args([
            "--query-gpu=index,name,memory.total,memory.used,utilization.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
    else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut devices = Vec::new();

    for line in stdout.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let parts = parse_csv_fields(line);
        if parts.len() < 5 {
            continue;
        }
        let index = parts[0].trim();
        devices.push(ComputeDevice {
            id: format!("nvidia:{index}"),
            kind: "discrete".to_string(),
            name: parts[1].trim().to_string(),
            vram_gb: parse_nvidia_number(&parts[2]).and_then(mb_to_gb),
            vram_used_gb: parse_nvidia_number(&parts[3]).and_then(mb_to_gb),
            util_pct: parse_util_pct(&parts[4]),
            enabled: true,
        });
    }

    devices
}

fn parse_csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in line.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    fields.push(current.trim().to_string());
    fields
}

fn parse_nvidia_number(raw: &str) -> Option<f32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("[N/A]") {
        return None;
    }
    trimmed.parse::<f32>().ok()
}

fn parse_util_pct(raw: &str) -> Option<u8> {
    let trimmed = raw.trim().trim_end_matches('%').trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("[N/A]") {
        return None;
    }
    trimmed
        .parse::<f32>()
        .ok()
        .map(|v| v.round().clamp(0.0, 100.0) as u8)
}

fn detect_amd_devices() -> Vec<ComputeDevice> {
    let Ok(output) = Command::new("rocm-smi")
        .args(["--showproductname"])
        .output()
    else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut names = Vec::new();
    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        if !(lower.contains("card series") || lower.contains("card model")) {
            continue;
        }
        if let Some(name) = line.rsplit(':').next().map(str::trim).filter(|v| !v.is_empty()) {
            names.push(name.to_string());
        }
    }

    let util_by_index = detect_amd_util_by_index();

    names
        .into_iter()
        .enumerate()
        .map(|(index, name)| ComputeDevice {
            id: format!("amd:{index}"),
            kind: "discrete".to_string(),
            name: format!("AMD {name}"),
            vram_gb: detect_amd_vram_gb(),
            vram_used_gb: None,
            util_pct: util_by_index.get(&index).copied(),
            enabled: true,
        })
        .collect()
}

fn detect_amd_util_by_index() -> std::collections::HashMap<usize, u8> {
    let Ok(output) = Command::new("rocm-smi").args(["--showuse"]).output() else {
        return std::collections::HashMap::new();
    };

    if !output.status.success() {
        return std::collections::HashMap::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut out = std::collections::HashMap::new();

    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        if !(lower.contains("gpu use") || lower.contains("gpu utilization")) {
            continue;
        }
        let index = line
            .split('[')
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .and_then(|value| value.trim().parse::<usize>().ok());
        let pct = line
            .split(':')
            .last()
            .and_then(parse_util_pct);
        if let (Some(index), Some(pct)) = (index, pct) {
            out.insert(index, pct);
        }
    }

    out
}

fn detect_integrated_pci_devices(existing: &[ComputeDevice]) -> Vec<ComputeDevice> {
    #[cfg(windows)]
    {
        return detect_integrated_windows_devices(existing);
    }
    #[cfg(unix)]
    {
        return detect_integrated_linux_pci_devices(existing);
    }
    #[cfg(not(any(unix, windows)))]
    {
        Vec::new()
    }
}

/// Discrete AMD / Intel Arc when vendor tools are absent (Linux lspci / Windows CIM).
fn detect_pci_vulkan_discrete_devices(existing: &[ComputeDevice]) -> Vec<ComputeDevice> {
    #[cfg(unix)]
    {
        return detect_pci_vulkan_discrete_linux(existing);
    }
    #[cfg(windows)]
    {
        return detect_pci_vulkan_discrete_windows(existing);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = existing;
        Vec::new()
    }
}

#[cfg(windows)]
fn detect_pci_vulkan_discrete_windows(existing: &[ComputeDevice]) -> Vec<ComputeDevice> {
    let output = powershell_hidden(&[
        "-Command",
        "Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name",
    ])
    .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    let known_names: HashSet<String> = existing
        .iter()
        .map(|d| d.name.to_ascii_lowercase())
        .collect();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut amd_i = 0usize;
    let mut intel_i = 0usize;
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|raw| {
            if known_names.contains(&raw.to_ascii_lowercase()) {
                return None;
            }
            if is_integrated_pci_name(raw) || !is_discrete_vulkan_pci_name(raw) {
                return None;
            }
            let lower = raw.to_ascii_lowercase();
            let (id, name) = if lower.contains("intel") || lower.contains("arc") {
                let id = format!("pci-intel:{intel_i}");
                intel_i += 1;
                (id, raw.to_string())
            } else {
                let id = format!("pci-amd:{amd_i}");
                amd_i += 1;
                (id, raw.to_string())
            };
            Some(ComputeDevice {
                id,
                kind: "discrete".to_string(),
                name,
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            })
        })
        .collect()
}

#[cfg(unix)]
fn detect_pci_vulkan_discrete_linux(existing: &[ComputeDevice]) -> Vec<ComputeDevice> {
    let output = match Command::new("lspci").output() {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    let known_names: HashSet<String> = existing
        .iter()
        .map(|d| d.name.to_ascii_lowercase())
        .collect();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let raw_names: Vec<String> = stdout
        .lines()
        .filter_map(|line| {
            let lower = line.to_ascii_lowercase();
            if !(lower.contains("vga compatible controller")
                || lower.contains("3d controller")
                || lower.contains("display controller"))
            {
                return None;
            }
            let name = line
                .split_once(':')
                .map(|(_, rest)| rest.trim())
                .filter(|value| !value.is_empty())?;
            if name.eq_ignore_ascii_case("device") {
                return None;
            }
            Some(name.to_string())
        })
        .collect();

    let names = dedupe_pci_gpu_names(raw_names);
    let mut amd_i = 0usize;
    let mut intel_i = 0usize;
    names
        .into_iter()
        .filter_map(|raw| {
            if is_integrated_pci_name(&raw) || !is_discrete_vulkan_pci_name(&raw) {
                return None;
            }
            let name = clean_pci_gpu_name(&raw);
            if known_names.contains(&name.to_ascii_lowercase()) {
                return None;
            }
            let lower = raw.to_ascii_lowercase();
            let (id, kind_name) = if lower.contains("intel") || lower.contains("arc") {
                let id = format!("pci-intel:{intel_i}");
                intel_i += 1;
                (id, name)
            } else {
                let id = format!("pci-amd:{amd_i}");
                amd_i += 1;
                (id, name)
            };
            Some(ComputeDevice {
                id,
                kind: "discrete".to_string(),
                name: kind_name,
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            })
        })
        .collect()
}

fn is_discrete_vulkan_pci_name(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("nvidia")
        || lower.contains("geforce")
        || lower.contains("quadro")
        || lower.contains("tesla")
    {
        return false;
    }
    lower.contains("amd")
        || lower.contains("radeon")
        || lower.contains("advanced micro devices")
        || lower.contains("arc")
}

#[cfg(unix)]
fn detect_integrated_linux_pci_devices(existing: &[ComputeDevice]) -> Vec<ComputeDevice> {
    let output = match Command::new("lspci").output() {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    let known_names: HashSet<String> = existing
        .iter()
        .map(|d| d.name.to_ascii_lowercase())
        .collect();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let raw_names: Vec<String> = stdout
        .lines()
        .filter_map(|line| {
            let lower = line.to_ascii_lowercase();
            if !(lower.contains("vga compatible controller")
                || lower.contains("3d controller")
                || lower.contains("display controller"))
            {
                return None;
            }
            let name = line
                .split_once(':')
                .map(|(_, rest)| rest.trim())
                .filter(|value| !value.is_empty())?;
            if name.eq_ignore_ascii_case("device") {
                return None;
            }
            Some(name.to_string())
        })
        .collect();

    let names = dedupe_pci_gpu_names(raw_names);
    names
        .into_iter()
        .enumerate()
        .filter_map(|(index, raw)| {
            let name = clean_pci_gpu_name(&raw);
            if known_names.contains(&name.to_ascii_lowercase()) {
                return None;
            }
            if !is_integrated_pci_name(&raw) {
                return None;
            }
            Some(ComputeDevice {
                id: format!("pci:{index}"),
                kind: "integrated".to_string(),
                name,
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: false,
            })
        })
        .collect()
}

#[cfg(windows)]
fn detect_integrated_windows_devices(existing: &[ComputeDevice]) -> Vec<ComputeDevice> {
    let output = powershell_hidden(&[
        "-Command",
        "Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name",
    ])
    .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    let known_names: HashSet<String> = existing
        .iter()
        .map(|d| d.name.to_ascii_lowercase())
        .collect();

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
        .filter_map(|(index, raw)| {
            if known_names.contains(&raw.to_ascii_lowercase()) {
                return None;
            }
            if !is_integrated_pci_name(raw) {
                return None;
            }
            Some(ComputeDevice {
                id: format!("pci:{index}"),
                kind: "integrated".to_string(),
                name: raw.to_string(),
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: false,
            })
        })
        .collect()
}

fn is_integrated_pci_name(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("nvidia")
        || lower.contains("geforce")
        || lower.contains("quadro")
        || lower.contains("arc") // discrete Intel Arc — not iGPU
    {
        return false;
    }
    // AMD discrete (RX / Pro / XT) is not integrated; APU lines use "Radeon Graphics".
    if (lower.contains("radeon") || lower.contains("amd"))
        && (lower.contains(" rx")
            || lower.contains("rx ")
            || lower.contains("pro ")
            || lower.contains(" xt")
            || lower.contains("xt ")
            || lower.contains("w ")
            || lower.contains("instinct"))
    {
        return false;
    }
    lower.contains("intel")
        || lower.contains("uhd")
        || lower.contains("iris")
        || lower.contains("hd graphics")
        || lower.contains("radeon graphics")
        || lower.contains("vega")
        || lower.contains("mali")
}

fn detect_cpu_device() -> Vec<ComputeDevice> {
    let Some(cpu_model) = detect_cpu_model() else {
        return Vec::new();
    };
    vec![ComputeDevice {
        id: "cpu:0".to_string(),
        kind: "cpu".to_string(),
        name: cpu_model,
        vram_gb: None,
        vram_used_gb: None,
        util_pct: None,
        enabled: false,
    }]
}

#[cfg(target_os = "macos")]
fn detect_apple_metal_device() -> Vec<ComputeDevice> {
    let ram_gb = detect_ram_gb().unwrap_or(8);
    let usable = apple_usable_gpu_gb(ram_gb);
    let cpu = detect_cpu_model().unwrap_or_else(|| "Apple Silicon".to_string());
    let name = if cpu.to_ascii_lowercase().contains("apple") {
        format!("{cpu} GPU")
    } else {
        format!("Apple Silicon GPU ({cpu})")
    };
    vec![ComputeDevice {
        id: "metal:0".to_string(),
        kind: "metal".to_string(),
        name,
        vram_gb: Some(usable),
        vram_used_gb: detect_ram_used_gb().map(|used| used.min(usable)),
        util_pct: None,
        enabled: true,
    }]
}

/// Unified memory minus OS/UI headroom — advertised as Metal "VRAM" for catalog fit.
#[cfg(target_os = "macos")]
fn apple_usable_gpu_gb(ram_gb: u32) -> u32 {
    let headroom = if ram_gb >= 64 {
        12
    } else if ram_gb >= 32 {
        8
    } else if ram_gb >= 16 {
        6
    } else {
        4
    };
    ram_gb.saturating_sub(headroom).max(1)
}

#[cfg(target_os = "macos")]
fn sysctl_string(name: &str) -> Option<String> {
    let c_name = std::ffi::CString::new(name).ok()?;
    let mut size: usize = 0;
    let rc = unsafe {
        libc::sysctlbyname(
            c_name.as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size];
    let rc = unsafe {
        libc::sysctlbyname(
            c_name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(size.saturating_sub(1)); // drop trailing NUL
    let s = String::from_utf8_lossy(&buf).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &str) -> Option<u64> {
    let c_name = std::ffi::CString::new(name).ok()?;
    let mut value: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    let rc = unsafe {
        libc::sysctlbyname(
            c_name.as_ptr(),
            &mut value as *mut u64 as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        Some(value)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn vm_stat_free_pages() -> Option<u64> {
    let output = Command::new("vm_stat").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut free = 0u64;
    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        if !(lower.contains("pages free") || lower.contains("pages speculative")) {
            continue;
        }
        if let Some(num) = line.split(':').nth(1) {
            let digits: String = num.chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<u64>() {
                free = free.saturating_add(n);
            }
        }
    }
    (free > 0).then_some(free)
}

fn detect_amd_vram_gb() -> Option<u32> {
    let output = Command::new("rocm-smi")
        .args(["--showmeminfo", "vram"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut best_mb = 0.0_f32;
    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        if !lower.contains("total") {
            continue;
        }
        for token in line.split_whitespace() {
            if let Ok(value) = token.parse::<f32>() {
                best_mb = best_mb.max(value);
            }
        }
    }

    mb_to_gb(best_mb)
}

fn dedupe_pci_gpu_names(raw_names: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in raw_names {
        let key = pci_gpu_dedupe_key(&raw);
        if seen.insert(key) {
            out.push(clean_pci_gpu_name(&raw));
        }
    }
    out
}

fn pci_gpu_dedupe_key(raw: &str) -> String {
    if let Some(start) = raw.find('[') {
        if let Some(end) = raw[start + 1..].find(']') {
            return raw[start + 1..start + 1 + end].to_ascii_lowercase();
        }
    }
    raw.split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn clean_pci_gpu_name(raw: &str) -> String {
    if let Some(start) = raw.find('[') {
        if let Some(end) = raw[start + 1..].find(']') {
            let inner = &raw[start + 1..start + 1 + end];
            if raw.to_ascii_lowercase().contains("nvidia") {
                return format!("NVIDIA {inner}");
            }
            if raw.to_ascii_lowercase().contains("amd") {
                return format!("AMD {inner}");
            }
            return inner.to_string();
        }
    }

    let mut name = raw.to_string();
    for prefix in [
        "NVIDIA Corporation ",
        "Advanced Micro Devices, Inc. [AMD/ATI] ",
        "Advanced Micro Devices, Inc. ",
        "Intel Corporation ",
    ] {
        if let Some(rest) = name.strip_prefix(prefix) {
            name = rest.to_string();
            break;
        }
    }

    name.trim().to_string()
}

pub fn detect_hostname() -> Option<String> {
    #[cfg(unix)]
    {
        if let Ok(host) = std::fs::read_to_string("/etc/hostname") {
            let trimmed = host.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }

    let mut hostname_cmd = Command::new("hostname");
    hide_console(&mut hostname_cmd);
    let output = hostname_cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }

    let host = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!host.is_empty()).then_some(host)
}

pub fn detect_cpu_model() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        return sysctl_string("machdep.cpu.brand_string");
    }
    #[cfg(target_os = "linux")]
    {
        return detect_cpu_model_linux();
    }
    #[cfg(windows)]
    {
        return detect_cpu_model_windows();
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn detect_cpu_model_linux() -> Option<String> {
    let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    info.lines()
        .find(|line| line.starts_with("model name"))
        .and_then(|line| line.split(':').nth(1))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(windows)]
fn detect_cpu_model_windows() -> Option<String> {
    let output = powershell_hidden(&[
        "-Command",
        "(Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name)",
    ])
    .output()
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

pub fn detect_ram_gb() -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        return sysctl_u64("hw.memsize").map(bytes_to_gb);
    }
    #[cfg(target_os = "linux")]
    {
        return detect_linux_memtotal_gb();
    }
    #[cfg(windows)]
    {
        return detect_windows_memtotal_gb();
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

pub fn detect_ram_used_gb() -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        let total = sysctl_u64("hw.memsize")?;
        // hw.memsize is physical RAM; vm_stat pages free is a coarse used estimate.
        let page = sysctl_u64("hw.pagesize").unwrap_or(16384);
        let free_pages = vm_stat_free_pages().unwrap_or(0);
        let used = total.saturating_sub(free_pages.saturating_mul(page));
        return Some(bytes_to_gb(used).max(1));
    }
    #[cfg(target_os = "linux")]
    {
        let total_kb = read_meminfo_kb("MemTotal:")?;
        let available_kb = read_meminfo_kb("MemAvailable:")?;
        let used_kb = total_kb.saturating_sub(available_kb);
        return Some(((used_kb as f64) / 1024.0 / 1024.0).round().max(1.0) as u32);
    }
    #[cfg(windows)]
    {
        let output = powershell_hidden(&[
            "-Command",
            "$os = Get-CimInstance Win32_OperatingSystem; [math]::Round(($os.TotalVisibleMemorySize - $os.FreePhysicalMemory) / 1MB, 0)",
        ])
        .output()
        .ok()?;
        if !output.status.success() {
            return None;
        }
        let used = String::from_utf8_lossy(&output.stdout).trim().parse::<u32>().ok()?;
        return Some(used.max(1));
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

fn bytes_to_gb(bytes: u64) -> u32 {
    ((bytes as f64) / 1024.0 / 1024.0 / 1024.0).round().max(1.0) as u32
}

fn bytes_to_gb_floor(bytes: u64) -> u32 {
    ((bytes as f64) / 1024.0 / 1024.0 / 1024.0).floor() as u32
}

/// Free bytes on the volume that holds the agent home / model cache.
pub fn disk_avail_bytes() -> Option<u64> {
    let path = crate::paths::home_dir().ok()?;
    disk_avail_bytes_for_path(&path)
}

/// True when less than 2 GiB is free — too little for another catalog GGUF.
pub fn disk_is_full() -> bool {
    const MIN_FREE: u64 = 2 * 1024 * 1024 * 1024;
    disk_avail_bytes().is_some_and(|avail| avail < MIN_FREE)
}

fn disk_usage_gb() -> Option<(Option<u32>, Option<u32>, Option<u32>)> {
    let path = crate::paths::home_dir().ok()?;
    disk_usage_for_path(&path)
}

fn disk_avail_bytes_for_path(path: &std::path::Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let bytes = path.as_os_str().as_bytes();
        let c_path = CString::new(bytes).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        return Some(stat.f_bavail as u64 * stat.f_frsize as u64);
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut free = 0u64;
        let mut total = 0u64;
        let mut total_free = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free as *mut u64,
                &mut total as *mut u64,
                &mut total_free as *mut u64,
            )
        };
        if ok == 0 {
            return None;
        }
        return Some(free);
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

fn disk_usage_for_path(path: &std::path::Path) -> Option<(Option<u32>, Option<u32>, Option<u32>)> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let bytes = path.as_os_str().as_bytes();
        let c_path = CString::new(bytes).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        let total_bytes = stat.f_blocks as u64 * stat.f_frsize as u64;
        let avail_bytes = stat.f_bavail as u64 * stat.f_frsize as u64;
        let total = bytes_to_gb(total_bytes);
        let used_bytes = total_bytes.saturating_sub(avail_bytes);
        let used = if used_bytes == 0 {
            0
        } else {
            bytes_to_gb(used_bytes).min(total)
        };
        return Some((Some(total), Some(used), Some(bytes_to_gb_floor(avail_bytes))));
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut free = 0u64;
        let mut total = 0u64;
        let mut total_free = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free as *mut u64,
                &mut total as *mut u64,
                &mut total_free as *mut u64,
            )
        };
        if ok == 0 || total == 0 {
            return None;
        }
        let total_gb = bytes_to_gb(total);
        let used_bytes = total.saturating_sub(free);
        let used_gb = if used_bytes == 0 {
            0
        } else {
            bytes_to_gb(used_bytes).min(total_gb)
        };
        return Some((
            Some(total_gb),
            Some(used_gb),
            Some(bytes_to_gb_floor(free)),
        ));
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

#[cfg(target_os = "linux")]
fn read_meminfo_kb(prefix: &str) -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    info.lines()
        .find(|line| line.starts_with(prefix))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u64>().ok())
}

#[cfg(target_os = "linux")]
fn detect_linux_memtotal_gb() -> Option<u32> {
    let kb = read_meminfo_kb("MemTotal:")?;
    Some(((kb as f64) / 1024.0 / 1024.0).round().max(1.0) as u32)
}

#[cfg(windows)]
fn detect_windows_memtotal_gb() -> Option<u32> {
    let output = powershell_hidden(&[
        "-Command",
        "[math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 0)",
    ])
    .output()
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let gb = String::from_utf8_lossy(&output.stdout).trim().parse::<u32>().ok()?;
    Some(gb.max(1))
}

pub fn detect_cuda_version() -> Option<String> {
    for bin in nvidia_smi_bins() {
        let output = match configure_nvidia_smi_command(&bin)
            .args(["--query-gpu=cuda_version", "--format=csv,noheader"])
            .output()
        {
            Ok(output) => output,
            Err(_) => continue,
        };

        if !output.status.success() {
            continue;
        }

        let version = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();

        if !version.is_empty() {
            return Some(version);
        }
    }
    None
}

pub fn detect_driver_version() -> Option<String> {
    for bin in nvidia_smi_bins() {
        let output = match configure_nvidia_smi_command(&bin)
            .args(["--query-gpu=driver_version", "--format=csv,noheader"])
            .output()
        {
            Ok(output) => output,
            Err(_) => continue,
        };

        if !output.status.success() {
            continue;
        }

        let version = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();

        if !version.is_empty() {
            return Some(version);
        }
    }
    None
}

fn mb_to_gb(mb: f32) -> Option<u32> {
    if mb <= 0.0 {
        return None;
    }
    Some(((mb / 1024.0).round() as u32).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrated_pci_names_are_detected() {
        assert!(is_integrated_pci_name(
            "Intel Corporation UHD Graphics 620 [8086:5917]"
        ));
        assert!(is_integrated_pci_name(
            "Advanced Micro Devices, Inc. [AMD/ATI] Picasso [Radeon Vega Series / Radeon Vega Mobile Series]"
        ));
        assert!(!is_integrated_pci_name(
            "NVIDIA Corporation GP107 [GeForce GTX 1650 SUPER]"
        ));
        assert!(!is_integrated_pci_name(
            "Advanced Micro Devices, Inc. [AMD/ATI] Navi 21 [Radeon RX 6800]"
        ));
        assert!(!is_integrated_pci_name(
            "Intel Corporation DG2 [Arc A770]"
        ));
    }

    #[test]
    fn discrete_vulkan_pci_names() {
        assert!(is_discrete_vulkan_pci_name(
            "Advanced Micro Devices, Inc. [AMD/ATI] Navi 21 [Radeon RX 6800]"
        ));
        assert!(is_discrete_vulkan_pci_name("Intel Corporation DG2 [Arc A770]"));
        assert!(!is_discrete_vulkan_pci_name(
            "Intel Corporation UHD Graphics 620"
        ));
        assert!(!is_discrete_vulkan_pci_name(
            "NVIDIA Corporation GP107 [GeForce GTX 1650 SUPER]"
        ));
        assert!(is_discrete_vulkan_pci_name("AMD Radeon RX 6800 XT"));
        assert!(is_discrete_vulkan_pci_name("Intel(R) Arc(TM) A770 Graphics"));
    }

    #[test]
    fn csv_fields_handle_commas_in_gpu_names() {
        let fields = parse_csv_fields(r#"0, "NVIDIA RTX A6000, v2", 49140, 1024, 37"#);
        assert_eq!(fields.len(), 5);
        assert_eq!(fields[1], "NVIDIA RTX A6000, v2");
        assert_eq!(parse_util_pct("37"), Some(37));
        assert_eq!(parse_util_pct("[N/A]"), None);
    }
}
