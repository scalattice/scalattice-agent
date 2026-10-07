//! OS install id, hostname, CPU, memory, and disk. GPU probing stays in the parent module.
use std::process::Command;
use std::sync::OnceLock;

pub(super) fn host_os_fields() -> (Option<String>, Option<String>, Option<String>) {
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
    let output = super::powershell_hidden(&[
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

pub(super) fn normalize_install_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches(|c| c == '{' || c == '}');
    if trimmed.len() < 8 || trimmed.len() > 80 {
        return None;
    }
    if !trimmed.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return None;
    }
    Some(trimmed.to_ascii_lowercase())
}

/// Stable id of this OS install. Cached for the process: it does not change at runtime.
pub fn detect_install_id() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE.get_or_init(read_install_id).clone()
}

fn read_install_id() -> Option<String> {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("reg");
        super::hide_console(&mut cmd);
        cmd.args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ]);
        let output = cmd.output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("MachineGuid") else {
                continue;
            };
            let Some(value) = rest.split_whitespace().last() else {
                continue;
            };
            if value.eq_ignore_ascii_case("REG_SZ") {
                continue;
            }
            return normalize_install_id(value);
        }
        return None;
    }

    #[cfg(target_os = "linux")]
    {
        for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Some(id) = normalize_install_id(&text) {
                    return Some(id);
                }
            }
        }
        return None;
    }

    #[cfg(target_os = "macos")]
    {
        let mut cmd = Command::new("ioreg");
        super::hide_console(&mut cmd);
        cmd.args(["-rd1", "-c", "IOPlatformExpertDevice"]);
        let output = cmd.output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("\"IOPlatformUUID\"") else {
                continue;
            };
            let Some(start) = rest.find('"') else {
                continue;
            };
            let after = &rest[start + 1..];
            let Some(end) = after.find('"') else { continue };
            return normalize_install_id(&after[..end]);
        }
        return None;
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        None
    }
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
    super::hide_console(&mut hostname_cmd);
    let output = hostname_cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }

    let host = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!host.is_empty()).then_some(host)
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

/// Windows `PROCESSOR_IDENTIFIER` is a raw Family/Model/Stepping string
/// (e.g. `AMD64 Family 25 Model 68 Stepping 1, AuthenticAMD`), not a product name.
#[cfg(any(test, windows))]
fn looks_like_processor_identifier(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    (lower.contains("family") && lower.contains("model") && lower.contains("stepping"))
        || lower.contains("authenticamd")
        || lower.contains("genuineintel")
}

#[cfg(windows)]
fn registry_processor_name() -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let subkey: Vec<u16> = "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0\0"
        .encode_utf16()
        .collect();
    let value: Vec<u16> = "ProcessorNameString\0".encode_utf16().collect();
    let mut buf = vec![0u16; 256];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut size,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let nchars = (size as usize / 2).saturating_sub(1).min(buf.len());
    let name = String::from_utf16_lossy(&buf[..nchars]).trim().to_string();
    if name.is_empty() || looks_like_processor_identifier(&name) {
        None
    } else {
        Some(name)
    }
}

#[cfg(windows)]
fn detect_cpu_model_windows() -> Option<String> {
    if let Some(name) = registry_processor_name() {
        return Some(name);
    }
    if let Ok(output) = super::powershell_hidden(&[
        "-Command",
        "(Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name)",
    ])
    .output()
    {
        if output.status.success() {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !name.is_empty() && !looks_like_processor_identifier(&name) {
                return Some(name);
            }
        }
    }
    None
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

pub(super) fn bytes_to_gb(bytes: u64) -> u32 {
    ((bytes as f64) / 1024.0 / 1024.0 / 1024.0).round().max(1.0) as u32
}

fn bytes_to_gb_floor(bytes: u64) -> u32 {
    ((bytes as f64) / 1024.0 / 1024.0 / 1024.0).floor() as u32
}

/// Free bytes on the volume that holds the agent home / model cache.
/// Prefer the models cache volume (where GGUF / Diffusers land). Fall back to home.
/// Windows boxes often put `~` on C: and models on D: — reporting home free space
/// while ENOSPC sticky-flagged the models drive made OSART33 look "disk full" at 192 GB free.
fn disk_inventory_path() -> Option<std::path::PathBuf> {
    let models = std::env::var("SCALATTICE_MODELS_DIR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(crate::paths::models_cache_dir);
    if models.exists() || models.parent().is_some_and(|p| p.exists()) {
        return Some(models);
    }
    crate::paths::home_dir().ok()
}

pub fn disk_avail_bytes() -> Option<u64> {
    disk_avail_bytes_for_path(&disk_inventory_path()?)
}

/// True when less than 2 GiB is free: too little for another catalog GGUF.
pub fn disk_is_full() -> bool {
    const MIN_FREE: u64 = 2 * 1024 * 1024 * 1024;
    disk_avail_bytes().is_some_and(|avail| avail < MIN_FREE)
}

/// Refresh the sticky disk-full flag from live free space on the models volume.
pub fn refresh_disk_full_flag() {
    crate::state::set_disk_full(disk_is_full());
}

pub(super) fn disk_usage_gb() -> Option<(Option<u32>, Option<u32>, Option<u32>)> {
    disk_usage_for_path(&disk_inventory_path()?)
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

/// Prefer Win32 `GlobalMemoryStatusEx`. PowerShell/WMI often fails for the
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
    let output = super::powershell_hidden(&[
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
    let output = super::powershell_hidden(&[
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let info = "processor\t: 0\nBogoMIPS\t: 50.00\nFeatures\t: fp asimd evtstrm aes\nCPU implementer\t: 0x41\nCPU architecture: 8\nCPU variant\t: 0x3\nCPU part\t: 0xd0c\nCPU revision\t: 1\n";
        assert_eq!(
            cpu_model_from_linux_cpuinfo(info).as_deref(),
            Some("ARM CPU (0x41 0xd0c)")
        );
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
    fn processor_identifier_is_not_a_cpu_product_name() {
        assert!(looks_like_processor_identifier(
            "AMD64 Family 25 Model 68 Stepping 1, AuthenticAMD"
        ));
        assert!(looks_like_processor_identifier(
            "Intel64 Family 6 Model 186 Stepping 3, GenuineIntel"
        ));
        assert!(!looks_like_processor_identifier(
            "AMD Ryzen 7 6800H with Radeon Graphics"
        ));
        assert!(!looks_like_processor_identifier("Apple M3"));
    }

    #[test]
    fn install_ids_are_hex_only() {
        assert_eq!(
            normalize_install_id("  A1B2C3D4-E5F6-7890-ABCD-EF1234567890  "),
            Some("a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string())
        );
        assert_eq!(
            normalize_install_id("{A1B2C3D4-E5F6-7890-ABCD-EF1234567890}"),
            Some("a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string())
        );
        assert_eq!(
            normalize_install_id("0123456789abcdef0123456789abcdef\n"),
            Some("0123456789abcdef0123456789abcdef".to_string())
        );
        assert_eq!(normalize_install_id("not an id"), None);
        assert_eq!(normalize_install_id("short"), None);
    }
}
