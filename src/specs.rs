use std::collections::HashSet;
use serde::Serialize;
use std::process::Command;

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
}

struct GpuSnapshot {
    name: Option<String>,
    vram_gb: Option<u32>,
    vram_used_gb: Option<u32>,
    util_pct: Option<u8>,
    count: Option<u8>,
    driver_version: Option<String>,
    cuda_version: Option<String>,
}

pub fn detect_machine_specs() -> MachineSpecs {
    let mut specs = MachineSpecs {
        hostname: detect_hostname(),
        cpu_model: detect_cpu_model(),
        ram_gb: detect_ram_gb(),
        ..MachineSpecs::default()
    };

    if let Some(gpu) = detect_gpus() {
        specs.gpu_name = gpu.name;
        specs.vram_gb = gpu.vram_gb;
        specs.vram_used_gb = gpu.vram_used_gb;
        specs.gpu_util_pct = gpu.util_pct;
        specs.gpu_count = gpu.count;
        specs.driver_version = gpu.driver_version;
        specs.cuda_version = gpu.cuda_version;
    }

    specs
}

pub fn status_line(specs: &MachineSpecs) -> String {
    match (&specs.gpu_name, specs.vram_gb) {
        (Some(name), Some(vram)) => {
            let util = specs
                .gpu_util_pct
                .map(|pct| format!(" · {pct}% load"))
                .unwrap_or_default();
            format!("GPU detected · {name} · {vram} GB VRAM{util}")
        }
        (Some(name), None) => format!("GPU detected · {name}"),
        (None, _) => "No GPU detected (install vendor tools or check drivers)".to_string(),
    }
}

fn detect_gpus() -> Option<GpuSnapshot> {
    detect_nvidia_gpus()
        .or_else(detect_amd_gpus)
        .or_else(detect_pci_gpus)
}

fn detect_nvidia_gpus() -> Option<GpuSnapshot> {
    for bin in [
        "/usr/bin/nvidia-smi",
        "/usr/sbin/nvidia-smi",
        "/usr/local/bin/nvidia-smi",
        "/usr/local/cuda/bin/nvidia-smi",
        "nvidia-smi",
    ] {
        if let Some(snapshot) = detect_nvidia_gpus_from(bin) {
            return Some(snapshot);
        }
    }
    None
}

fn detect_nvidia_gpus_from(bin: &str) -> Option<GpuSnapshot> {
    let output = Command::new(bin)
        .args([
            "--query-gpu=index,name,memory.total,memory.used,utilization.gpu,driver_version",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<String> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();

    if lines.is_empty() {
        return None;
    }

    let mut best: Option<(u32, usize)> = None;
    for (idx, line) in lines.iter().enumerate() {
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts.len() < 5 {
            continue;
        }
        let vram_gb = parts[2].parse::<f32>().ok().and_then(mb_to_gb);
        let score = vram_gb.unwrap_or(0);
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, idx));
        }
    }

    let (idx, count) = match best {
        Some((_, idx)) => (idx, lines.len().min(255) as u8),
        None => return None,
    };

    let parts: Vec<&str> = lines[idx].split(',').map(str::trim).collect();
    let vram_gb = parts[2].parse::<f32>().ok().and_then(mb_to_gb);
    let vram_used_gb = parts[3].parse::<f32>().ok().and_then(mb_to_gb);
    let util_pct = parts[4]
        .parse::<f32>()
        .ok()
        .map(|v| v.round().clamp(0.0, 100.0) as u8);

    Some(GpuSnapshot {
        name: Some(parts[1].to_string()),
        vram_gb,
        vram_used_gb,
        util_pct,
        count: Some(count),
        driver_version: parts
            .get(5)
            .map(|value| value.to_string())
            .filter(|value| !value.is_empty()),
        cuda_version: detect_cuda_version(),
    })
}

fn detect_cuda_version() -> Option<String> {
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

fn detect_amd_gpus() -> Option<GpuSnapshot> {
    let output = Command::new("rocm-smi")
        .args(["--showproductname"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
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

    if names.is_empty() {
        return None;
    }

    Some(GpuSnapshot {
        name: Some(format!("AMD {}", names[0])),
        vram_gb: detect_amd_vram_gb(),
        vram_used_gb: None,
        util_pct: None,
        count: Some(names.len().min(255) as u8),
        driver_version: Some("ROCm".to_string()),
        cuda_version: None,
    })
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

fn detect_pci_gpus() -> Option<GpuSnapshot> {
    let output = Command::new("lspci").output().ok()?;
    if !output.status.success() {
        return None;
    }

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

    if names.is_empty() {
        return None;
    }

    Some(GpuSnapshot {
        name: Some(names[0].clone()),
        vram_gb: None,
        vram_used_gb: None,
        util_pct: None,
        count: Some(names.len().min(255) as u8),
        driver_version: None,
        cuda_version: None,
    })
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
    ] {
        if let Some(rest) = name.strip_prefix(prefix) {
            name = rest.to_string();
            break;
        }
    }

  name.trim().to_string()
}

fn detect_hostname() -> Option<String> {
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

fn detect_cpu_model() -> Option<String> {
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

fn mb_to_gb(mb: f32) -> Option<u32> {
    if mb <= 0.0 {
        return None;
    }
    Some(((mb / 1024.0).round() as u32).max(1))
}
