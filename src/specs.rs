use serde::{Deserialize, Serialize};
#[cfg(not(target_os = "macos"))]
use std::collections::HashSet;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

mod host;
#[allow(unused_imports)]
pub use host::disk_avail_bytes;
use host::{bytes_to_gb, disk_usage_gb, host_os_fields};
pub use host::{
    detect_cpu_model, detect_hostname, detect_install_id, detect_ram_gb, detect_ram_used_gb,
    disk_is_full,
};

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
    /// OS install id (Windows MachineGuid, Linux machine-id, macOS platform UUID).
    /// Reported so the server can recognize the same PC after a new account.
    /// A modified agent can send a different value.
    #[serde(rename = "installId", skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
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
    #[serde(rename = "os", skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(rename = "osVersion", skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    #[serde(rename = "osPretty", skip_serializing_if = "Option::is_none")]
    pub os_pretty: Option<String>,
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
        // Unified memory: CPU is not a second compute device beside Metal.
        if !devices.iter().any(|d| d.kind == "metal") {
            devices.extend(detect_cpu_device());
        }
        for device in &mut devices {
            if device.kind == "metal" {
                device.enabled = true;
            }
        }
        enable_cpu_if_no_accelerator(&mut devices);
        prefer_cpu_only_for_update_smoke(&mut devices);
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
        enable_cpu_if_no_accelerator(&mut devices);
        prefer_cpu_only_for_update_smoke(&mut devices);

        return devices;
    }

    #[allow(unreachable_code)]
    Vec::new()
}

/// CPU is off by default when a GPU exists. With no accelerator (typical GitHub
/// runner, CPU-only provider), leave it off and the supervisor exits immediately.
/// systemd Restart=always then crash-loops and the agent never reaches Cloud.
fn is_accelerator_kind(kind: &str) -> bool {
    kind == "discrete" || kind == "integrated" || kind == "metal"
}

fn enable_cpu_if_no_accelerator(devices: &mut Vec<ComputeDevice>) {
    if devices.iter().any(|d| d.enabled) {
        return;
    }
    if let Some(cpu) = devices.iter_mut().find(|d| d.kind == "cpu") {
        cpu.enabled = true;
        return;
    }
    let mut cpu = fallback_cpu_device();
    cpu.enabled = true;
    devices.push(cpu);
}

/// Update smoke only needs a live Cloud session. Pin to CPU so CI does not
/// initialize Metal/CUDA (slow, and on Windows it would fight the host agent).
///
/// macOS production omits `cpu:0` beside Metal. Smoke still has to inject CPU
/// *before* disabling accelerators, or supervisor start fails with nothing
/// enabled and the agent never registers with the mock Cloud WS.
fn prefer_cpu_only_for_update_smoke(devices: &mut Vec<ComputeDevice>) {
    if !crate::config::update_smoke_test() {
        return;
    }
    pin_devices_to_cpu_only(devices);
}

fn pin_devices_to_cpu_only(devices: &mut Vec<ComputeDevice>) {
    if !devices.iter().any(|d| d.kind == "cpu") {
        devices.push(fallback_cpu_device());
    }
    for device in devices.iter_mut() {
        device.enabled = device.kind == "cpu";
    }
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
        let (os, os_version, os_pretty) = host_os_fields();
        return MachineSpecs {
            agent_version: Some(agent_version_string()),
            hostname,
            cpu_model,
            ram_gb,
            ram_used_gb: detect_ram_used_gb(),
            disk_total_gb: disk.0,
            disk_used_gb: disk.1,
            disk_free_gb: disk.2,
            os,
            os_version,
            os_pretty,
            install_id: detect_install_id(),
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
    let accelerators: Vec<&ComputeDevice> = enabled
        .iter()
        .copied()
        .filter(|d| is_accelerator_kind(&d.kind))
        .collect();
    let discrete_count = accelerators.iter().filter(|d| d.kind == "discrete").count();

    // GPU line is accelerators only: do not concatenate the CPU brand into it.
    let gpu_name = if accelerators.len() == 1 {
        Some(accelerators[0].name.clone())
    } else if accelerators.len() > 1 {
        Some(
            accelerators
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(" + "),
        )
    } else {
        None
    };

    let vram_gb = sum_option(accelerators.iter().filter_map(|d| d.vram_gb));
    let vram_used_gb = sum_option(enabled.iter().filter_map(|d| d.vram_used_gb));
    let gpu_util_pct = enabled.iter().filter_map(|d| d.util_pct).max();

    let gpu_count = if discrete_count > 0 {
        Some(discrete_count.min(255) as u8)
    } else if !accelerators.is_empty() {
        Some(accelerators.len().min(255) as u8)
    } else {
        None
    };

    let disk = disk_usage_gb().unwrap_or((None, None, None));
    let (os, os_version, os_pretty) = host_os_fields();

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
        os,
        os_version,
        os_pretty,
        install_id: detect_install_id(),
        compute_devices: devices.to_vec(),
    }
}

pub fn status_line(specs: &MachineSpecs) -> String {
    let enabled: Vec<_> = specs.compute_devices.iter().filter(|d| d.enabled).collect();

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
    if total > 0 {
        Some(total)
    } else {
        None
    }
}

#[cfg(not(target_os = "macos"))]
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

fn wsl_nvidia_lib_dir() -> Option<&'static str> {
    if std::path::Path::new("/usr/lib/wsl/lib").is_dir() {
        Some("/usr/lib/wsl/lib")
    } else {
        None
    }
}

/// Physical CUDA indices from `CUDA_VISIBLE_DEVICES` (worker pin).
/// Empty if unset, blank, or `-1` (hide all devices).
fn cuda_visible_physical_indices() -> Vec<u32> {
    let Ok(raw) = std::env::var("CUDA_VISIBLE_DEVICES") else {
        return Vec::new();
    };
    if raw.trim().is_empty() || raw.trim() == "-1" {
        return Vec::new();
    }
    raw.split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect()
}

/// Live free VRAM (GiB) for this process's pinned NVIDIA GPU(s).
///
/// Only queries when `CUDA_VISIBLE_DEVICES` pins real device indices.
/// Returns the **minimum** free across visible devices.
pub fn live_cuda_free_vram_gb() -> Option<f64> {
    #[cfg(target_os = "macos")]
    {
        let _ = cuda_visible_physical_indices();
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        let wanted = cuda_visible_physical_indices();
        if wanted.is_empty() {
            return None;
        }
        let map = live_cuda_free_vram_by_index();
        wanted
            .iter()
            .filter_map(|idx| map.get(idx).copied())
            .reduce(f64::min)
    }
}

/// Live free VRAM (GiB) for every NVIDIA GPU nvidia-smi can see.
/// Used by the supervisor at placement time (no `CUDA_VISIBLE_DEVICES`).
pub fn live_cuda_free_vram_by_index() -> std::collections::HashMap<u32, f64> {
    #[cfg(target_os = "macos")]
    {
        std::collections::HashMap::new()
    }
    #[cfg(not(target_os = "macos"))]
    {
        for bin in nvidia_smi_bins() {
            let map = live_cuda_free_all_from(&bin);
            if !map.is_empty() {
                return map;
            }
        }
        std::collections::HashMap::new()
    }
}

#[cfg(not(target_os = "macos"))]
fn live_cuda_free_all_from(bin: &str) -> std::collections::HashMap<u32, f64> {
    let mut map = std::collections::HashMap::new();
    let Some(output) = configure_nvidia_smi_command(bin)
        .args([
            "--query-gpu=index,memory.free",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()
    else {
        return map;
    };
    if !output.status.success() {
        return map;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let parts = parse_csv_fields(line);
        if parts.len() < 2 {
            continue;
        }
        let Ok(index) = parts[0].trim().parse::<u32>() else {
            continue;
        };
        let Some(mb) = parse_nvidia_number(&parts[1]) else {
            continue;
        };
        map.insert(index, f64::from(mb) / 1024.0);
    }
    map
}

/// NVIDIA compute capability as major*10+minor (Ampere 8.0 → 80, Turing 7.5 → 75).
/// Uses the process's pinned `CUDA_VISIBLE_DEVICES` GPU(s); `None` if unset or unknown.
pub fn live_cuda_compute_cap() -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        static CACHED: OnceLock<Option<u32>> = OnceLock::new();
        *CACHED.get_or_init(|| {
            let wanted = cuda_visible_physical_indices();
            if wanted.is_empty() {
                return None;
            }
            for bin in nvidia_smi_bins() {
                if let Some(cap) = live_cuda_compute_cap_from(&bin, &wanted) {
                    return Some(cap);
                }
            }
            None
        })
    }
}

#[cfg(not(target_os = "macos"))]
fn live_cuda_compute_cap_from(bin: &str, wanted: &[u32]) -> Option<u32> {
    let output = configure_nvidia_smi_command(bin)
        .args(["--query-gpu=index,compute_cap", "--format=csv,noheader"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut min_cap: Option<u32> = None;
    for line in stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let parts = parse_csv_fields(line);
        if parts.len() < 2 {
            continue;
        }
        let Ok(index) = parts[0].trim().parse::<u32>() else {
            continue;
        };
        if !wanted.contains(&index) {
            continue;
        }
        let Some(cap) = parse_compute_cap(parts[1].trim()) else {
            continue;
        };
        min_cap = Some(match min_cap {
            None => cap,
            Some(prev) => prev.min(cap),
        });
    }
    min_cap
}

#[cfg(any(test, not(target_os = "macos")))]
pub(crate) fn parse_compute_cap(raw: &str) -> Option<u32> {
    let s = raw.trim();
    let mut parts = s.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next().unwrap_or("0").chars().next()?.to_digit(10)?;
    Some(major.saturating_mul(10).saturating_add(minor))
}

/// Turing / Pascal / Maxwell consumer and datacenter names. Ampere+ is false.
pub fn nvidia_name_is_pre_ampere(name: &str) -> bool {
    let n = name.to_ascii_lowercase().replace('-', " ");
    if n.contains("gtx") {
        return true;
    }
    if n.contains("rtx 20") || n.contains("rtx20") {
        return true;
    }
    if n.contains("titan rtx") || n.contains("titan x") || n.contains("titan v") {
        return true;
    }
    if n.contains("tesla t4")
        || n.contains("tesla p")
        || n.contains("tesla k")
        || n.contains("tesla m")
    {
        return true;
    }
    // RTX Axxxx / RTX Ada are Ampere+; un-prefixed "Quadro RTX 4000" is Turing.
    if n.contains("rtx a") || n.contains("rtx ada") {
        return false;
    }
    if n.contains("quadro rtx") {
        return true;
    }
    n.contains("quadro t")
        || n.contains("quadro p")
        || n.contains("quadro k")
        || n.contains("quadro m")
}

/// Live free VRAM (GiB) for an AMD GPU via `rocm-smi`. `gpu_index` is the
/// `amd:N` id; `None` uses the smallest free across cards.
pub fn live_rocm_free_vram_gb(gpu_index: Option<usize>) -> Option<f64> {
    #[cfg(target_os = "macos")]
    {
        let _ = gpu_index;
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        let output = hide_console(&mut Command::new("rocm-smi"))
            .args(["--showmeminfo", "vram"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let free = parse_amd_vram_free_gib(&String::from_utf8_lossy(&output.stdout));
        if let Some(index) = gpu_index {
            return free.get(&index).copied();
        }
        free.values().copied().reduce(f64::min)
    }
}

/// Live free Metal budget (GiB) from unified memory. None off macOS.
///
/// Do **not** use `vm_stat` free pages here. Metal weights live in the same
/// RAM those pages measure, so a 64 GB M1 Max with a model loaded reports ~5 GB
/// "free" while llama.cpp still sees ~52 GB. That made us evict every resident
/// on every model switch.
pub fn live_metal_free_vram_gb() -> Option<f64> {
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
    #[cfg(target_os = "macos")]
    {
        let ram = detect_ram_gb()?;
        Some(f64::from(apple_usable_gpu_gb(ram)))
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

#[cfg(not(target_os = "macos"))]
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

#[cfg(not(target_os = "macos"))]
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

    for line in stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
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

#[cfg(any(test, not(target_os = "macos")))]
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

#[cfg(not(target_os = "macos"))]
fn parse_nvidia_number(raw: &str) -> Option<f32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("[N/A]") {
        return None;
    }
    trimmed.parse::<f32>().ok()
}

#[cfg(any(test, not(target_os = "macos")))]
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

#[cfg(not(target_os = "macos"))]
fn detect_amd_devices() -> Vec<ComputeDevice> {
    let Ok(output) = hide_console(&mut Command::new("rocm-smi"))
        .args(["--showproductname"])
        .output()
    else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    parse_amd_devices_from_rocm(
        &String::from_utf8_lossy(&output.stdout),
        &detect_amd_vram_by_index(),
        &detect_amd_util_by_index(),
    )
}

#[cfg(any(test, not(target_os = "macos")))]
#[derive(Default)]
struct AmdGpuDraft {
    series: Option<String>,
    model: Option<String>,
}

/// Group rocm-smi `--showproductname` by GPU[N]. Card series and card model are
/// two lines for the same device: treating each line as a GPU duplicated the
/// 7900 XTX as both "RX 7900 XTX" and "0x744c".
#[cfg(any(test, not(target_os = "macos")))]
fn parse_amd_devices_from_rocm(
    product_stdout: &str,
    vram_by_index: &std::collections::HashMap<usize, u32>,
    util_by_index: &std::collections::HashMap<usize, u8>,
) -> Vec<ComputeDevice> {
    use std::collections::BTreeMap;

    let mut by_index: BTreeMap<usize, AmdGpuDraft> = BTreeMap::new();
    for line in product_stdout.lines() {
        let Some(index) = parse_rocm_gpu_index(line) else {
            continue;
        };
        let lower = line.to_ascii_lowercase();
        let value = parse_rocm_field_value(line);
        let entry = by_index.entry(index).or_default();
        if lower.contains("card series") || lower.contains("device name") {
            if let Some(value) = value {
                entry.series = Some(value);
            }
        } else if lower.contains("card model") {
            if let Some(value) = value {
                entry.model = Some(value);
            }
        }
    }

    by_index
        .into_iter()
        .map(|(index, draft)| {
            let raw_name = amd_display_name(draft.series.as_deref(), draft.model.as_deref())
                .unwrap_or_else(|| format!("GPU {index}"));
            let name = if raw_name.to_ascii_lowercase().starts_with("amd") {
                raw_name
            } else {
                format!("AMD {raw_name}")
            };
            let integrated = is_integrated_pci_name(&name);
            ComputeDevice {
                id: format!("amd:{index}"),
                kind: if integrated {
                    "integrated".to_string()
                } else {
                    "discrete".to_string()
                },
                name,
                vram_gb: vram_by_index.get(&index).copied(),
                vram_used_gb: None,
                util_pct: util_by_index.get(&index).copied(),
                enabled: !integrated,
            }
        })
        .collect()
}

#[cfg(any(test, not(target_os = "macos")))]
fn amd_display_name(series: Option<&str>, model: Option<&str>) -> Option<String> {
    let series = series.map(str::trim).filter(|v| !v.is_empty());
    let model = model.map(str::trim).filter(|v| !v.is_empty());
    match (series, model) {
        (Some(series), _) if !looks_like_pci_id(series) => Some(series.to_string()),
        (_, Some(model)) if !looks_like_pci_id(model) => Some(model.to_string()),
        (Some(series), _) => Some(series.to_string()),
        (_, Some(model)) => Some(model.to_string()),
        _ => None,
    }
}

#[cfg(any(test, not(target_os = "macos")))]
fn looks_like_pci_id(raw: &str) -> bool {
    let hex = raw
        .trim()
        .strip_prefix("0x")
        .or_else(|| raw.trim().strip_prefix("0X"))
        .unwrap_or(raw.trim());
    (4..=6).contains(&hex.len()) && hex.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(any(test, not(target_os = "macos")))]
fn parse_rocm_gpu_index(line: &str) -> Option<usize> {
    let start = line.find("GPU[")?;
    let rest = &line[start + 4..];
    let end = rest.find(']')?;
    rest[..end].trim().parse().ok()
}

#[cfg(any(test, not(target_os = "macos")))]
fn parse_rocm_field_value(line: &str) -> Option<String> {
    line.rsplit_once(':')
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn detect_amd_util_by_index() -> std::collections::HashMap<usize, u8> {
    let Ok(output) = hide_console(&mut Command::new("rocm-smi"))
        .args(["--showuse"])
        .output()
    else {
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
        let index = parse_rocm_gpu_index(line);
        let pct = line.split(':').last().and_then(parse_util_pct);
        if let (Some(index), Some(pct)) = (index, pct) {
            out.insert(index, pct);
        }
    }

    out
}

#[cfg(not(target_os = "macos"))]
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
#[cfg(not(target_os = "macos"))]
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

#[cfg(all(unix, not(target_os = "macos")))]
fn detect_pci_vulkan_discrete_linux(existing: &[ComputeDevice]) -> Vec<ComputeDevice> {
    let output = match Command::new("lspci").output() {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    let known_names: HashSet<String> = existing
        .iter()
        .map(|d| d.name.to_ascii_lowercase())
        .collect();
    let has_rocm_amd = existing.iter().any(|d| d.id.starts_with("amd:"));

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
                // rocm-smi already enumerated AMD GPUs: skip lspci duplicates.
                if has_rocm_amd {
                    return None;
                }
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

#[cfg(any(test, not(target_os = "macos")))]
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

#[cfg(all(unix, not(target_os = "macos")))]
fn detect_integrated_linux_pci_devices(existing: &[ComputeDevice]) -> Vec<ComputeDevice> {
    let output = match Command::new("lspci").output() {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    let known_names: HashSet<String> = existing
        .iter()
        .map(|d| d.name.to_ascii_lowercase())
        .collect();
    let has_rocm_amd = existing.iter().any(|d| d.id.starts_with("amd:"));

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
            let lower = raw.to_ascii_lowercase();
            if has_rocm_amd
                && (lower.contains("amd")
                    || lower.contains("radeon")
                    || lower.contains("advanced micro devices"))
            {
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

#[cfg(any(test, not(target_os = "macos")))]
fn is_integrated_pci_name(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("nvidia")
        || lower.contains("geforce")
        || lower.contains("quadro")
        || lower.contains("arc")
    // discrete Intel Arc: not iGPU
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
        && !lower.contains("radeon graphics")
        && !lower.contains("890m")
        && !lower.contains("780m")
        && !lower.contains("760m")
        && !lower.contains("740m")
        && !lower.contains("680m")
        && !lower.contains("660m")
    {
        return false;
    }
    lower.contains("intel")
        || lower.contains("uhd")
        || lower.contains("iris")
        || lower.contains("hd graphics")
        || lower.contains("radeon graphics")
        || lower.contains("890m")
        || lower.contains("780m")
        || lower.contains("760m")
        || lower.contains("740m")
        || lower.contains("680m")
        || lower.contains("660m")
        || lower.contains("vega")
        || lower.contains("mali")
}

fn detect_cpu_device() -> Vec<ComputeDevice> {
    vec![fallback_cpu_device()]
}

fn fallback_cpu_device() -> ComputeDevice {
    ComputeDevice {
        id: "cpu:0".to_string(),
        kind: "cpu".to_string(),
        name: detect_cpu_model().unwrap_or_else(|| "CPU".to_string()),
        vram_gb: None,
        vram_used_gb: None,
        util_pct: None,
        enabled: false,
    }
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

/// OS/UI RAM that must stay off the Metal GPU budget, and off a second GGUF mmap.
/// 12.5% of installed RAM, at least 4 GB so the OS still has a floor on 8 GB machines.
pub(crate) fn system_ram_reserve_gb(ram_gb: u32) -> f64 {
    (f64::from(ram_gb) * 0.125).max(4.0)
}

/// Unified memory minus OS/UI headroom: advertised as Metal "VRAM" for catalog fit.
/// This is `ram - reserve(ram)`, not a 16/32/64 GB size class.
#[cfg(any(test, target_os = "macos"))]
pub(crate) fn apple_usable_gpu_gb(ram_gb: u32) -> u32 {
    ram_gb
        .saturating_sub(system_ram_reserve_gb(ram_gb).round() as u32)
        .max(1)
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

#[cfg(not(target_os = "macos"))]
fn detect_amd_vram_by_index() -> std::collections::HashMap<usize, u32> {
    let mut out = std::collections::HashMap::new();
    let Ok(output) = hide_console(&mut Command::new("rocm-smi"))
        .args(["--showmeminfo", "vram"])
        .output()
    else {
        return out;
    };
    if !output.status.success() {
        return out;
    }
    parse_amd_vram_by_index(&String::from_utf8_lossy(&output.stdout), &mut out);
    out
}

/// Current rocm-smi prints VRAM in bytes (`(B): 25753026560`). Older builds
/// used megabytes. Treating bytes as MB advertised ~25 million GB per card.
#[cfg(any(test, not(target_os = "macos")))]
fn parse_amd_vram_by_index(stdout: &str, out: &mut std::collections::HashMap<usize, u32>) {
    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("used") || !lower.contains("total") {
            continue;
        }
        let Some(index) = parse_rocm_gpu_index(line) else {
            continue;
        };
        if let Some(gb) = parse_rocm_mem_line_gb(line) {
            out.insert(index, gb);
        }
    }
}

#[cfg(any(test, not(target_os = "macos")))]
fn parse_rocm_mem_line_gb(line: &str) -> Option<u32> {
    let lower = line.to_ascii_lowercase();
    let mut value: Option<f64> = None;
    for token in line.split_whitespace() {
        let token = token.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        if token.is_empty() {
            continue;
        }
        if let Ok(parsed) = token.parse::<f64>() {
            value = Some(parsed);
        }
    }
    let value = value.filter(|v| *v > 0.0)?;
    if lower.contains("(b)")
        || lower.contains("bytes")
        || (value >= 1_000_000.0 && !lower.contains("(mb)") && !lower.contains("(gb)"))
    {
        return Some(bytes_to_gb(value as u64));
    }
    if lower.contains("(gb)") {
        return Some(value.round().max(1.0) as u32);
    }
    mb_to_gb(value as f32)
}

/// GiB from a rocm-smi meminfo line (no 1 GB floor: used/free can be fractional).
#[cfg(any(test, not(target_os = "macos")))]
fn parse_rocm_mem_line_gib(line: &str) -> Option<f64> {
    let lower = line.to_ascii_lowercase();
    let mut value: Option<f64> = None;
    for token in line.split_whitespace() {
        let token = token.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        if token.is_empty() {
            continue;
        }
        if let Ok(parsed) = token.parse::<f64>() {
            value = Some(parsed);
        }
    }
    let value = value.filter(|v| *v >= 0.0)?;
    if lower.contains("(b)")
        || lower.contains("bytes")
        || (value >= 1_000_000.0 && !lower.contains("(mb)") && !lower.contains("(gb)"))
    {
        return Some(value / 1024.0 / 1024.0 / 1024.0);
    }
    if lower.contains("(gb)") {
        return Some(value);
    }
    Some(value / 1024.0)
}

#[cfg(any(test, not(target_os = "macos")))]
fn parse_amd_vram_free_gib(stdout: &str) -> std::collections::HashMap<usize, f64> {
    let mut total = std::collections::HashMap::new();
    let mut used = std::collections::HashMap::new();
    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        let Some(index) = parse_rocm_gpu_index(line) else {
            continue;
        };
        let Some(gib) = parse_rocm_mem_line_gib(line) else {
            continue;
        };
        if lower.contains("used") {
            used.insert(index, gib);
        } else if lower.contains("total") {
            total.insert(index, gib);
        }
    }
    total
        .into_iter()
        .map(|(index, tot)| {
            let used_gb = used.get(&index).copied().unwrap_or(0.0);
            (index, (tot - used_gb).max(0.0))
        })
        .collect()
}

#[cfg(all(unix, not(target_os = "macos")))]
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

#[cfg(all(unix, not(target_os = "macos")))]
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

#[cfg(all(unix, not(target_os = "macos")))]
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

/// CUDA runtime linked into this binary. Bump these when the build's toolkit
/// changes. They are not a product rule of their own: a driver that cannot run
/// this runtime means the agent is not compatible with the graphics cards.
pub const BUNDLED_CUDA_MAJOR: u32 = 12;
pub const BUNDLED_CUDA_MINOR: u32 = 6;

static ACCEL_COMPAT_WATCH: AtomicBool = AtomicBool::new(false);
static ACCEL_INCOMPATIBLE: AtomicBool = AtomicBool::new(false);
static ACCEL_PTX_JIT_MISSING: AtomicBool = AtomicBool::new(false);

pub fn accelerator_incompatible_message() -> &'static str {
    if ACCEL_PTX_JIT_MISSING.load(Ordering::Relaxed) {
        return "This graphics card cannot run jobs: NVIDIA's PTX compiler library is missing. Install the driver package that includes it. GPU containers need that library mounted in.";
    }
    "This agent isn't compatible with the graphics driver. Update the driver so the graphics cards can run jobs."
}

/// GPU workers call this before backend init. The processor slot must not,
/// because llama.cpp probes CUDA there too.
pub fn arm_accelerator_compat_watch() {
    ACCEL_COMPAT_WATCH.store(true, Ordering::Relaxed);
}

pub fn mark_accelerator_incompatible() {
    ACCEL_INCOMPATIBLE.store(true, Ordering::Relaxed);
}

pub fn accelerator_runtime_incompatible() -> bool {
    ACCEL_INCOMPATIBLE.load(Ordering::Relaxed)
}

#[cfg(test)]
pub fn reset_accelerator_compat_for_test() {
    ACCEL_COMPAT_WATCH.store(false, Ordering::Relaxed);
    ACCEL_INCOMPATIBLE.store(false, Ordering::Relaxed);
    ACCEL_PTX_JIT_MISSING.store(false, Ordering::Relaxed);
}

/// Parent-side: a worker stderr line, including after the worker has already aborted.
/// Does not use the GPU-worker watch, so a CPU slot probing CUDA cannot flag the machine.
pub fn note_worker_stderr_line(msg: &str) {
    if msg
        .to_ascii_lowercase()
        .contains("ptx jit compiler library not found")
    {
        ACCEL_PTX_JIT_MISSING.store(true, Ordering::Relaxed);
        mark_accelerator_incompatible();
    }
}

/// True when a graphics worker's own log says its backend cannot run.
pub fn note_accelerator_log_line(msg: &str) {
    if !ACCEL_COMPAT_WATCH.load(Ordering::Relaxed) {
        return;
    }
    let lower = msg.to_ascii_lowercase();
    if lower.contains("ptx jit compiler library not found") {
        ACCEL_PTX_JIT_MISSING.store(true, Ordering::Relaxed);
        mark_accelerator_incompatible();
        return;
    }
    let incompatible = lower.contains("driver version is insufficient")
        || lower.contains("failed to initialize cuda")
        || lower.contains("failed to initialize vulkan")
        || lower.contains("failed to initialize metal");
    if incompatible {
        mark_accelerator_incompatible();
    }
}

pub fn parse_cuda_major_minor(raw: &str) -> Option<(u32, u32)> {
    let nums: Vec<u32> = raw
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .take(2)
        .collect();
    let major = *nums.first()?;
    let minor = nums.get(1).copied().unwrap_or(0);
    Some((major, minor))
}

/// True only when the driver reported a CUDA version older than the runtime
/// linked into this binary. Missing or unreadable versions are not a fault.
pub fn nvidia_driver_too_old(cuda_version: Option<&str>) -> bool {
    let Some(raw) = cuda_version
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let Some(found) = parse_cuda_major_minor(raw) else {
        return false;
    };
    found < (BUNDLED_CUDA_MAJOR, BUNDLED_CUDA_MINOR)
}

#[cfg(not(target_os = "macos"))]
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
    fn graphics_backend_log_marks_the_agent_incompatible() {
        reset_accelerator_compat_for_test();
        assert!(!accelerator_runtime_incompatible());
        note_accelerator_log_line("failed to initialize CUDA: CUDA driver version is insufficient");
        assert!(!accelerator_runtime_incompatible());
        arm_accelerator_compat_watch();
        note_accelerator_log_line("llama.cpp backend ready");
        assert!(!accelerator_runtime_incompatible());
        note_accelerator_log_line(
            "failed to initialize CUDA: CUDA driver version is insufficient for CUDA runtime version",
        );
        assert!(accelerator_runtime_incompatible());
        reset_accelerator_compat_for_test();
    }

    #[test]
    fn ptx_jit_missing_is_a_distinct_driver_fault() {
        reset_accelerator_compat_for_test();
        arm_accelerator_compat_watch();
        note_accelerator_log_line("CUDA error: PTX JIT compiler library not found");
        assert!(accelerator_runtime_incompatible());
        assert!(accelerator_incompatible_message().contains("PTX compiler library"));
        reset_accelerator_compat_for_test();
    }

    #[test]
    fn older_nvidia_cuda_is_too_old_for_this_agent() {
        assert!(nvidia_driver_too_old(Some("12.2")));
        assert!(nvidia_driver_too_old(Some("11.8")));
        assert!(!nvidia_driver_too_old(Some("12.6")));
        assert!(!nvidia_driver_too_old(Some("12.6.3")));
        assert!(!nvidia_driver_too_old(Some("13.0")));
        assert!(!nvidia_driver_too_old(None));
        assert!(!nvidia_driver_too_old(Some("")));
    }

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
        assert!(!is_integrated_pci_name("Intel Corporation DG2 [Arc A770]"));
    }

    #[test]
    fn apple_usable_gpu_scales_with_unified_memory_not_size_classes() {
        // reserve = max(ram * 12.5%, 4 GB); usable = ram - round(reserve)
        assert_eq!(apple_usable_gpu_gb(8), 4);
        assert_eq!(apple_usable_gpu_gb(16), 12);
        assert_eq!(apple_usable_gpu_gb(18), 14);
        assert_eq!(apple_usable_gpu_gb(24), 20);
        assert_eq!(apple_usable_gpu_gb(32), 28);
        assert_eq!(apple_usable_gpu_gb(36), 31);
        assert_eq!(apple_usable_gpu_gb(48), 42);
        assert_eq!(apple_usable_gpu_gb(64), 56);
        assert_eq!(apple_usable_gpu_gb(96), 84);
        assert_eq!(apple_usable_gpu_gb(128), 112);
        assert_eq!(apple_usable_gpu_gb(192), 168);
        let reserve = system_ram_reserve_gb(64);
        assert!((reserve - 8.0).abs() < 0.01);
        assert_eq!(
            apple_usable_gpu_gb(64),
            64u32.saturating_sub(reserve.round() as u32)
        );
    }

    #[test]
    fn discrete_vulkan_pci_names() {
        assert!(is_discrete_vulkan_pci_name(
            "Advanced Micro Devices, Inc. [AMD/ATI] Navi 21 [Radeon RX 6800]"
        ));
        assert!(is_discrete_vulkan_pci_name(
            "Intel Corporation DG2 [Arc A770]"
        ));
        assert!(!is_discrete_vulkan_pci_name(
            "Intel Corporation UHD Graphics 620"
        ));
        assert!(!is_discrete_vulkan_pci_name(
            "NVIDIA Corporation GP107 [GeForce GTX 1650 SUPER]"
        ));
        assert!(is_discrete_vulkan_pci_name("AMD Radeon RX 6800 XT"));
        assert!(is_discrete_vulkan_pci_name(
            "Intel(R) Arc(TM) A770 Graphics"
        ));
    }

    #[test]
    fn csv_fields_handle_commas_in_gpu_names() {
        let fields = parse_csv_fields(r#"0, "NVIDIA RTX A6000, v2", 49140, 1024, 37"#);
        assert_eq!(fields.len(), 5);
        assert_eq!(fields[1], "NVIDIA RTX A6000, v2");
        assert_eq!(parse_util_pct("37"), Some(37));
        assert_eq!(parse_util_pct("[N/A]"), None);
    }

    #[test]
    fn compute_cap_and_pre_ampere_names() {
        assert_eq!(parse_compute_cap("7.5"), Some(75));
        assert_eq!(parse_compute_cap("8.6"), Some(86));
        assert_eq!(parse_compute_cap("12.0"), Some(120));
        assert!(nvidia_name_is_pre_ampere("NVIDIA GeForce GTX 1660 SUPER"));
        assert!(nvidia_name_is_pre_ampere("GeForce RTX 2080 Ti"));
        assert!(nvidia_name_is_pre_ampere("Tesla T4"));
        assert!(!nvidia_name_is_pre_ampere("NVIDIA GeForce RTX 3080"));
        assert!(!nvidia_name_is_pre_ampere("NVIDIA RTX A6000"));
        assert!(!nvidia_name_is_pre_ampere("NVIDIA GeForce RTX 4090"));
    }

    #[test]
    fn cpu_is_enabled_when_nothing_else_is_present() {
        let mut devices = Vec::new();
        enable_cpu_if_no_accelerator(&mut devices);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].kind, "cpu");
        assert!(devices[0].enabled);
    }

    #[test]
    fn update_smoke_pins_cpu_even_when_macos_omits_it_beside_metal() {
        let mut devices = vec![ComputeDevice {
            id: "metal:0".to_string(),
            kind: "metal".to_string(),
            name: "Apple M3 GPU".to_string(),
            vram_gb: Some(16),
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        }];
        pin_devices_to_cpu_only(&mut devices);
        assert_eq!(devices.len(), 2);
        let metal = devices.iter().find(|d| d.kind == "metal").expect("metal");
        let cpu = devices.iter().find(|d| d.kind == "cpu").expect("cpu");
        assert!(!metal.enabled);
        assert!(cpu.enabled);
        assert!(devices.iter().any(|d| d.enabled));
    }

    #[test]
    fn amd_rocm_groups_series_and_model_per_gpu() {
        let product = r#"
======================= ROCm System Management Interface =======================
GPU[0]		: Card series: 	 Radeon RX 7900 XTX
GPU[0]		: Card model: 	 0x744c
GPU[1]		: Card series: 	 AMD Radeon Graphics
GPU[1]		: Card model: 	 0x150e
================================================================================
"#;
        let mut vram = std::collections::HashMap::new();
        vram.insert(0, 24);
        vram.insert(1, 2);
        let util = std::collections::HashMap::new();
        let devices = parse_amd_devices_from_rocm(product, &vram, &util);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].id, "amd:0");
        assert_eq!(devices[0].name, "AMD Radeon RX 7900 XTX");
        assert_eq!(devices[0].kind, "discrete");
        assert_eq!(devices[0].vram_gb, Some(24));
        assert!(devices[0].enabled);
        assert_eq!(devices[1].id, "amd:1");
        assert_eq!(devices[1].name, "AMD Radeon Graphics");
        assert_eq!(devices[1].kind, "integrated");
        assert_eq!(devices[1].vram_gb, Some(2));
        assert!(!devices[1].enabled);
    }

    #[test]
    fn amd_vram_bytes_are_not_treated_as_megabytes() {
        let stdout = r#"
GPU[0]		: VRAM Total Memory (B): 25753026560
GPU[0]		: VRAM Total Used Memory (B): 123456
GPU[1]		: VRAM Total Memory (B): 2147483648
"#;
        let mut out = std::collections::HashMap::new();
        parse_amd_vram_by_index(stdout, &mut out);
        assert_eq!(out.get(&0).copied(), Some(24));
        assert_eq!(out.get(&1).copied(), Some(2));
        let free = parse_amd_vram_free_gib(stdout);
        let g0 = free.get(&0).copied().unwrap();
        assert!((g0 - 24.0).abs() < 0.2, "free {g0}");
        let g1 = free.get(&1).copied().unwrap();
        assert!((g1 - 2.0).abs() < 0.2, "free {g1}");
    }

    #[test]
    fn amd_vram_legacy_megabytes_still_parse() {
        let stdout = "GPU[0]\t: Total Memory (MB): 24576\n";
        let mut out = std::collections::HashMap::new();
        parse_amd_vram_by_index(stdout, &mut out);
        assert_eq!(out.get(&0).copied(), Some(24));
    }

    #[test]
    fn amd_igpu_names_are_integrated() {
        assert!(is_integrated_pci_name("AMD Radeon Graphics"));
        assert!(is_integrated_pci_name(
            "AMD Ryzen AI 9 HX PRO 370 w/ Radeon 890M"
        ));
        assert!(!is_integrated_pci_name("AMD Radeon RX 7900 XTX"));
    }
}
