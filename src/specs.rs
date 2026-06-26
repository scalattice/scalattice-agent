use std::collections::HashSet;
use serde::{Deserialize, Serialize};
use std::process::Command;

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
    #[serde(rename = "computeDevices", skip_serializing_if = "Vec::is_empty")]
    pub compute_devices: Vec<ComputeDevice>,
}

pub fn detect_all_compute_devices() -> Vec<ComputeDevice> {
    let mut devices = Vec::new();
    devices.extend(detect_nvidia_devices());
    devices.extend(detect_amd_devices());
    devices.extend(detect_integrated_pci_devices(&devices));
    devices.extend(detect_cpu_device());

    for device in &mut devices {
        if device.kind == "discrete" {
            device.enabled = true;
        }
    }

    devices
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
    let mut devices = detect_all_compute_devices();
    if devices.is_empty() {
        return MachineSpecs {
            hostname,
            cpu_model,
            ram_gb,
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

    let vram_gb = sum_option(enabled.iter().filter_map(|d| d.vram_gb));
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

    MachineSpecs {
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
            if total > 0 {
                format!("{total} compute device(s) detected (none enabled)")
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
    for bin in [
        "/usr/bin/nvidia-smi",
        "/usr/sbin/nvidia-smi",
        "/usr/local/bin/nvidia-smi",
        "/usr/local/cuda/bin/nvidia-smi",
        "nvidia-smi",
    ] {
        let devices = detect_nvidia_devices_from(bin);
        if !devices.is_empty() {
            return devices;
        }
    }
    Vec::new()
}

fn detect_nvidia_devices_from(bin: &str) -> Vec<ComputeDevice> {
    let Ok(output) = Command::new(bin)
        .args([
            "--query-gpu=index,name,memory.total,memory.used,utilization.gpu,driver_version",
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
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts.len() < 5 {
            continue;
        }
        let index = parts[0];
        devices.push(ComputeDevice {
            id: format!("nvidia:{index}"),
            kind: "discrete".to_string(),
            name: parts[1].to_string(),
            vram_gb: parts[2].parse::<f32>().ok().and_then(mb_to_gb),
            vram_used_gb: parts[3].parse::<f32>().ok().and_then(mb_to_gb),
            util_pct: parts[4]
                .parse::<f32>()
                .ok()
                .map(|v| v.round().clamp(0.0, 100.0) as u8),
            enabled: true,
        });
    }

    devices
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

    names
        .into_iter()
        .enumerate()
        .map(|(index, name)| ComputeDevice {
            id: format!("amd:{index}"),
            kind: "discrete".to_string(),
            name: format!("AMD {name}"),
            vram_gb: detect_amd_vram_gb(),
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        })
        .collect()
}

fn detect_integrated_pci_devices(existing: &[ComputeDevice]) -> Vec<ComputeDevice> {
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

fn is_integrated_pci_name(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("nvidia") || lower.contains("geforce") || lower.contains("quadro") {
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
    let ram_gb = detect_ram_gb();
    vec![ComputeDevice {
        id: "cpu:0".to_string(),
        kind: "cpu".to_string(),
        name: cpu_model,
        vram_gb: ram_gb,
        vram_used_gb: None,
        util_pct: None,
        enabled: false,
    }]
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
    if let Ok(host) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = host.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }

    let output = Command::new("hostname").output().ok()?;
    if !output.status.success() {
        return None;
    }

    let host = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!host.is_empty()).then_some(host)
}

pub fn detect_cpu_model() -> Option<String> {
    let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let model = info
        .lines()
        .find(|line| line.starts_with("model name"))
        .and_then(|line| line.split(':').nth(1))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)?;

    Some(model)
}

fn detect_ram_gb() -> Option<u32> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb = info
        .lines()
        .find(|line| line.starts_with("MemTotal:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u64>().ok())?;

    Some(((kb as f64) / 1024.0 / 1024.0).round().max(1.0) as u32)
}

pub fn detect_cuda_version() -> Option<String> {
    for bin in [
        "/usr/bin/nvidia-smi",
        "/usr/sbin/nvidia-smi",
        "/usr/local/bin/nvidia-smi",
        "nvidia-smi",
    ] {
        let output = match Command::new(bin)
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
    for bin in [
        "/usr/bin/nvidia-smi",
        "/usr/sbin/nvidia-smi",
        "/usr/local/bin/nvidia-smi",
        "nvidia-smi",
    ] {
        let output = match Command::new(bin)
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
        assert!(!is_integrated_pci_name(
            "NVIDIA Corporation GP107 [GeForce GTX 1650 SUPER]"
        ));
    }
}
