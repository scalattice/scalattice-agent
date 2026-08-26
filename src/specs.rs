use serde::{Deserialize, Serialize};
#[cfg(not(target_os = "macos"))]
use std::collections::HashSet;
use std::process::Command;
use std::sync::OnceLock;

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
        devices.extend(detect_cpu_device());
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
/// runner, CPU-only provider), leave it off and the hypervisor exits immediately
/// — systemd Restart=always then crash-loops and the agent never reaches Cloud.
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
fn prefer_cpu_only_for_update_smoke(devices: &mut Vec<ComputeDevice>) {
    if !crate::config::update_smoke_test() {
        return;
    }
    enable_cpu_if_no_accelerator(devices);
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
    let discrete_count = enabled.iter().filter(|d| d.kind == "discrete").count();

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
    let gpu_util_pct = enabled.iter().filter_map(|d| d.util_pct).max();

    let gpu_count = if discrete_count > 0 {
        Some(discrete_count.min(255) as u8)
    } else if !enabled.is_empty() {
        Some(enabled.len().min(255) as u8)
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

/// First working `nvidia-smi` for diagnostics (absolute path preferred).
#[allow(dead_code)]
pub fn resolve_nvidia_smi() -> Option<String> {
    for bin in nvidia_smi_bins() {
        if bin != "nvidia-smi" && bin != "nvidia-smi.exe" && !std::path::Path::new(&bin).is_file() {
            continue;
        }
        let Ok(output) = configure_nvidia_smi_command(&bin).args(["-L"]).output() else {
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
/// two lines for the same device — treating each line as a GPU duplicated the
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
                // rocm-smi already enumerated AMD GPUs — skip lspci duplicates.
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
    // discrete Intel Arc — not iGPU
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
    if lower.contains("(b)") || lower.contains("bytes") || (value >= 1_000_000.0 && !lower.contains("(mb)") && !lower.contains("(gb)"))
    {
        return Some(bytes_to_gb(value as u64));
    }
    if lower.contains("(gb)") {
        return Some(value.round().max(1.0) as u32);
    }
    mb_to_gb(value as f32)
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

fn host_os_fields() -> (Option<String>, Option<String>, Option<String>) {
    static CACHED: OnceLock<(Option<String>, Option<String>, Option<String>)> = OnceLock::new();
    CACHED.get_or_init(detect_host_os).clone()
}

fn detect_host_os() -> (Option<String>, Option<String>, Option<String>) {
    let os = std::env::consts::OS.to_string();
    #[cfg(target_os = "linux")]
    {
        if let Some((version, pretty)) = detect_os_linux() {
            return (Some(os), version, Some(pretty));
        }
    }
    #[cfg(windows)]
    {
        if let Some((version, pretty)) = detect_os_windows() {
            return (Some(os), version, Some(pretty));
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some((version, pretty)) = detect_os_macos() {
            return (Some(os), version, Some(pretty));
        }
    }
    (
        Some(os.clone()),
        None,
        Some(os_family_label(&os).to_string()),
    )
}

fn os_family_label(os: &str) -> &'static str {
    match os {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        _ => "Unknown",
    }
}

#[cfg(any(test, target_os = "linux"))]
fn unquote_os_release_value(raw: &str) -> String {
    let trimmed = raw.trim();
    if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2)
    {
        return trimmed[1..trimmed.len() - 1].trim().to_string();
    }
    trimmed.to_string()
}

#[cfg(any(test, target_os = "linux"))]
fn parse_os_release(raw: &str) -> (Option<String>, Option<String>, Option<String>) {
    let mut name = None;
    let mut version_id = None;
    let mut pretty = None;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unquote_os_release_value(value);
        if value.is_empty() {
            continue;
        }
        match key {
            "NAME" => name = Some(value),
            "VERSION_ID" => version_id = Some(value),
            "PRETTY_NAME" => pretty = Some(value),
            _ => {}
        }
    }
    (name, version_id, pretty)
}

#[cfg(any(test, target_os = "linux"))]
fn pretty_linux(name: Option<&str>, version_id: Option<&str>, pretty: Option<&str>) -> String {
    match (
        name.map(str::trim).filter(|s| !s.is_empty()),
        version_id.map(str::trim).filter(|s| !s.is_empty()),
    ) {
        (Some(name), Some(version)) => format!("{name} {version}"),
        _ => pretty
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| name.map(str::to_string))
            .unwrap_or_else(|| "Linux".to_string()),
    }
}

#[cfg(any(test, windows))]
fn pretty_windows(caption: &str, version: &str) -> String {
    let lower = caption.to_ascii_lowercase();
    if lower.contains("windows 11") {
        return "Windows 11".to_string();
    }
    if lower.contains("windows 10") {
        return "Windows 10".to_string();
    }
    if let Some(build) = version
        .split('.')
        .nth(2)
        .and_then(|part| part.parse::<u32>().ok())
    {
        if build >= 22000 {
            return "Windows 11".to_string();
        }
        if build >= 10240 {
            return "Windows 10".to_string();
        }
    }
    let stripped = caption.trim().trim_start_matches("Microsoft ").trim();
    if stripped.is_empty() {
        "Windows".to_string()
    } else {
        stripped.to_string()
    }
}

#[cfg(any(test, target_os = "macos"))]
fn macos_codename(major: u32) -> Option<&'static str> {
    match major {
        11 => Some("Big Sur"),
        12 => Some("Monterey"),
        13 => Some("Ventura"),
        14 => Some("Sonoma"),
        15 => Some("Sequoia"),
        16 | 26 => Some("Tahoe"),
        _ => None,
    }
}

#[cfg(any(test, target_os = "macos"))]
fn pretty_macos(version: &str) -> String {
    let major = version
        .split('.')
        .next()
        .and_then(|part| part.parse::<u32>().ok());
    if let Some(name) = major.and_then(macos_codename) {
        format!("macOS {name}")
    } else if version.trim().is_empty() {
        "macOS".to_string()
    } else {
        format!("macOS {version}")
    }
}

#[cfg(target_os = "linux")]
fn detect_os_linux() -> Option<(Option<String>, String)> {
    let raw = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .ok()?;
    let (name, version_id, pretty_name) = parse_os_release(&raw);
    let pretty = pretty_linux(
        name.as_deref(),
        version_id.as_deref(),
        pretty_name.as_deref(),
    );
    Some((version_id, pretty))
}

#[cfg(windows)]
fn detect_os_windows() -> Option<(Option<String>, String)> {
    let output = powershell_hidden(&[
        "-Command",
        "$o = Get-CimInstance Win32_OperatingSystem | Select-Object -First 1; \"$($o.Caption)|$($o.Version)\"",
    ])
    .output()
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let line = line.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return None;
    }
    let (caption, version) = line.split_once('|').unwrap_or((line, ""));
    let version = version.trim();
    Some((
        (!version.is_empty()).then(|| version.to_string()),
        pretty_windows(caption.trim(), version),
    ))
}

#[cfg(target_os = "macos")]
fn detect_os_macos() -> Option<(Option<String>, String)> {
    let output = Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        return None;
    }
    Some((Some(version.clone()), pretty_macos(&version)))
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
    if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo") {
        if let Some(name) = cpu_model_from_linux_cpuinfo(&info) {
            return Some(name);
        }
    }
    for path in [
        "/sys/firmware/devicetree/base/model",
        "/proc/device-tree/model",
    ] {
        if let Ok(raw) = std::fs::read_to_string(path) {
            let model = raw.trim_matches('\0').trim();
            if !model.is_empty() {
                return Some(model.to_string());
            }
        }
    }
    None
}

#[cfg(any(target_os = "linux", test))]
fn cpu_model_from_linux_cpuinfo(info: &str) -> Option<String> {
    for key in ["model name", "Hardware"] {
        if let Some(value) = cpuinfo_field(info, key) {
            return Some(value);
        }
    }
    let implementer = cpuinfo_field(info, "CPU implementer");
    let part = cpuinfo_field(info, "CPU part");
    match (implementer, part) {
        (Some(imp), Some(part)) => Some(format!("ARM CPU ({imp} {part})")),
        (_, Some(part)) => Some(format!("ARM CPU ({part})")),
        (Some(imp), _) => Some(format!("ARM CPU ({imp})")),
        _ => None,
    }
}

#[cfg(any(target_os = "linux", test))]
fn cpuinfo_field(info: &str, key: &str) -> Option<String> {
    for line in info.lines() {
        let Some((left, right)) = line.split_once(':') else {
            continue;
        };
        if !left.trim().eq_ignore_ascii_case(key) {
            continue;
        }
        let value = right.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(windows)]
fn detect_cpu_model_windows() -> Option<String> {
    if let Ok(output) = powershell_hidden(&[
        "-Command",
        "(Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name)",
    ])
    .output()
    {
        if output.status.success() {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    std::env::var("PROCESSOR_IDENTIFIER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
        return detect_windows_memused_gb();
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
        return Some((
            Some(total),
            Some(used),
            Some(bytes_to_gb_floor(avail_bytes)),
        ));
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
        return Some((Some(total_gb), Some(used_gb), Some(bytes_to_gb_floor(free))));
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

/// Prefer Win32 `GlobalMemoryStatusEx` — PowerShell/WMI often fails for the
/// Windows service account (no interactive session / CIM blocked), which made
/// capacity checks see 0 GB RAM and reject every model.
#[cfg(windows)]
fn windows_memory_status() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok == 0 || status.ullTotalPhys == 0 {
        return None;
    }
    Some((status.ullTotalPhys, status.ullAvailPhys))
}

#[cfg(windows)]
fn detect_windows_memtotal_gb() -> Option<u32> {
    if let Some((total, _)) = windows_memory_status() {
        return Some(bytes_to_gb(total));
    }
    // Fallback when the Win32 call is unavailable (rare).
    let output = powershell_hidden(&[
        "-Command",
        "[math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 0)",
    ])
    .output()
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let gb = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .ok()?;
    Some(gb.max(1))
}

#[cfg(windows)]
fn detect_windows_memused_gb() -> Option<u32> {
    if let Some((total, avail)) = windows_memory_status() {
        let used = total.saturating_sub(avail);
        return Some(bytes_to_gb(used).max(1).min(bytes_to_gb(total)));
    }
    let output = powershell_hidden(&[
        "-Command",
        "$os = Get-CimInstance Win32_OperatingSystem; [math]::Round(($os.TotalVisibleMemorySize - $os.FreePhysicalMemory) / 1MB, 0)",
    ])
    .output()
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let used = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .ok()?;
    Some(used.max(1))
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
    fn linux_os_release_pretty_uses_name_and_version() {
        let raw = r#"
NAME="Ubuntu"
VERSION_ID="22.04"
PRETTY_NAME="Ubuntu 22.04.5 LTS"
"#;
        let (name, version, pretty) = parse_os_release(raw);
        assert_eq!(name.as_deref(), Some("Ubuntu"));
        assert_eq!(version.as_deref(), Some("22.04"));
        assert_eq!(
            pretty_linux(name.as_deref(), version.as_deref(), pretty.as_deref()),
            "Ubuntu 22.04"
        );
    }

    #[test]
    fn arm_cpuinfo_without_model_name_still_names_the_cpu() {
        let info = r#"
processor	: 0
BogoMIPS	: 50.00
Features	: fp asimd evtstrm aes
CPU implementer	: 0x41
CPU architecture: 8
CPU variant	: 0x3
CPU part	: 0xd0c
CPU revision	: 1
"#;
        assert_eq!(
            cpu_model_from_linux_cpuinfo(info).as_deref(),
            Some("ARM CPU (0x41 0xd0c)")
        );
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
    fn windows_and_macos_pretty_names() {
        assert_eq!(
            pretty_windows("Microsoft Windows 11 Pro", "10.0.22631"),
            "Windows 11"
        );
        assert_eq!(
            pretty_windows("Windows 10 Home", "10.0.19045"),
            "Windows 10"
        );
        assert_eq!(pretty_macos("15.1"), "macOS Sequoia");
        assert_eq!(pretty_macos("14.6.1"), "macOS Sonoma");
        assert_eq!(pretty_macos("26.0"), "macOS Tahoe");
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
        assert!(is_integrated_pci_name("AMD Ryzen AI 9 HX PRO 370 w/ Radeon 890M"));
        assert!(!is_integrated_pci_name("AMD Radeon RX 7900 XTX"));
    }
}
