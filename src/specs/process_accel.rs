//! Cross-platform per-process accelerator memory / util samples.
//!
//! Vendor CLIs (`nvidia-smi`, `rocm-smi`) are preferred when they work.
//! On Windows WDDM they usually return empty — fall back to GPU performance
//! counters (same source as Task Manager). On Linux, DRM fdinfo covers Intel /
//! AMD / Nouveau when vendor tools miss a pid. macOS Metal uses unified memory
//! (RSS + device IOAccelerator); there is no public per-process VRAM API.

use std::collections::HashMap;

/// Fill `dst` with entries from `src` only when the key is missing (or 0).
pub(crate) fn merge_fill_u32(dst: &mut HashMap<(u32, u32), u32>, src: HashMap<(u32, u32), u32>) {
    for (key, mb) in src {
        if mb == 0 {
            continue;
        }
        dst.entry(key).or_insert(mb);
    }
}

/// Fill util map; keep the higher % when both sources report a pid.
pub(crate) fn merge_fill_util(dst: &mut HashMap<(u32, u32), u8>, src: HashMap<(u32, u32), u8>) {
    for (key, pct) in src {
        dst.entry(key)
            .and_modify(|v| *v = (*v).max(pct))
            .or_insert(pct);
    }
}

/// OS-generic samples used when NVIDIA/AMD CLIs are empty or incomplete.
pub(crate) fn live_os_process_vram_mb_by_gpu_pid(
    interest_pids: &[u32],
) -> HashMap<(u32, u32), u32> {
    #[cfg(windows)]
    {
        let _ = interest_pids;
        return live_windows_pdh_process_vram_mb();
    }
    #[cfg(target_os = "linux")]
    {
        return live_linux_drm_process_vram_mb(interest_pids);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = interest_pids;
        HashMap::new()
    }
    #[cfg(all(
        not(windows),
        not(target_os = "linux"),
        not(target_os = "macos")
    ))]
    {
        let _ = interest_pids;
        HashMap::new()
    }
}

pub(crate) fn live_os_process_util_pct_by_gpu_pid(
    interest_pids: &[u32],
) -> HashMap<(u32, u32), u8> {
    #[cfg(windows)]
    {
        let _ = interest_pids;
        return live_windows_pdh_process_util_pct();
    }
    #[cfg(target_os = "linux")]
    {
        return live_linux_drm_process_util_pct(interest_pids);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = interest_pids;
        HashMap::new()
    }
    #[cfg(all(
        not(windows),
        not(target_os = "linux"),
        not(target_os = "macos")
    ))]
    {
        let _ = interest_pids;
        HashMap::new()
    }
}

// ─── Windows: Task Manager counters (NVIDIA / AMD / Intel WDDM) ─────────────

#[cfg(windows)]
fn live_windows_pdh_process_vram_mb() -> HashMap<(u32, u32), u32> {
    // Same counters Task Manager uses. nvidia-smi compute-apps is N/A under WDDM.
    let script = r#"
$ErrorActionPreference = 'SilentlyContinue'
$out = New-Object System.Collections.Generic.List[string]
function Emit-Mem($samples) {
  foreach ($s in $samples) {
    $n = [string]$s.InstanceName
    if ($n -notmatch 'pid_(\d+)') { continue }
    $procId = [uint32]$Matches[1]
    if ($procId -eq 0) { continue }
    $gpu = 0
    if ($n -match 'phys_(\d+)') { $gpu = [uint32]$Matches[1] }
    $mb = [uint32][math]::Round([double]$s.CookedValue / 1MB)
    if ($mb -gt 0) { [void]$out.Add(("{0},{1},{2}" -f $gpu, $procId, $mb)) }
  }
}
$hit = $false
foreach ($path in @('\GPU Process Memory(*)\Dedicated Usage','\GPU Process Memory(*)\Local Usage')) {
  $c = Get-Counter -Counter $path -ErrorAction SilentlyContinue
  if ($c -and $c.CounterSamples -and $c.CounterSamples.Count -gt 0) {
    Emit-Mem $c.CounterSamples
    $hit = $true
    break
  }
}
if (-not $hit) {
  $list = Get-Counter -ListSet '*GPU*Process*Memory*' -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($list) {
    $paths = @($list.PathsWithInstances | Where-Object {
      $_ -match 'Dedicated Usage$' -or $_ -match 'Local Usage$' -or $_ -match 'Dedicated$' -or $_ -match 'Local$'
    })
    if (-not $paths -or $paths.Count -eq 0) { $paths = @($list.PathsWithInstances) }
    if ($paths -and $paths.Count -gt 0) {
      $c = Get-Counter -Counter ($paths | Select-Object -First 64) -ErrorAction SilentlyContinue
      if ($c -and $c.CounterSamples) { Emit-Mem $c.CounterSamples }
    }
  }
}
$out -join "`n"
"#;
    parse_gpu_pid_mb_csv(&run_powershell(script))
}

#[cfg(windows)]
fn live_windows_pdh_process_util_pct() -> HashMap<(u32, u32), u8> {
    let script = r#"
$ErrorActionPreference = 'SilentlyContinue'
$out = New-Object System.Collections.Generic.List[string]
function Emit-Util($samples) {
  $best = @{}
  foreach ($s in $samples) {
    $n = [string]$s.InstanceName
    if ($n -notmatch 'pid_(\d+)') { continue }
    $procId = [uint32]$Matches[1]
    if ($procId -eq 0) { continue }
    $gpu = 0
    if ($n -match 'phys_(\d+)') { $gpu = [uint32]$Matches[1] }
    $pct = [double]$s.CookedValue
    if ($pct -lt 0) { continue }
    if ($pct -gt 100) { $pct = 100 }
    $key = "{0}|{1}" -f $gpu, $procId
    if (-not $best.ContainsKey($key) -or $pct -gt $best[$key]) { $best[$key] = $pct }
  }
  foreach ($k in $best.Keys) {
    $parts = $k.Split('|')
    $rounded = [uint32][math]::Round($best[$k])
    if ($rounded -gt 0) { [void]$out.Add(("{0},{1},{2}" -f $parts[0], $parts[1], $rounded)) }
  }
}
$c = Get-Counter -Counter '\GPU Engine(*)\Utilization Percentage' -ErrorAction SilentlyContinue
if ($c -and $c.CounterSamples -and $c.CounterSamples.Count -gt 0) {
  Emit-Util $c.CounterSamples
} else {
  $list = Get-Counter -ListSet '*GPU*Engine*' -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($list) {
    $paths = @($list.PathsWithInstances | Where-Object { $_ -match 'Utilization' })
    if ($paths -and $paths.Count -gt 0) {
      $c = Get-Counter -Counter ($paths | Select-Object -First 128) -ErrorAction SilentlyContinue
      if ($c -and $c.CounterSamples) { Emit-Util $c.CounterSamples }
    }
  }
}
$out -join "`n"
"#;
    parse_gpu_pid_util_csv(&run_powershell(script))
}

#[cfg(windows)]
fn run_powershell(script: &str) -> String {
    let output = match super::powershell_hidden(&["-Command", script]).output() {
        Ok(o) => o,
        Err(_) => return String::new(),
    };
    if !output.status.success() && output.stdout.is_empty() {
        return String::new();
    }
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[cfg(any(test, windows))]
fn parse_gpu_pid_mb_csv(stdout: &str) -> HashMap<(u32, u32), u32> {
    let mut map = HashMap::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.trim().split(',').map(str::trim).collect();
        if parts.len() < 3 {
            continue;
        }
        let Ok(gpu) = parts[0].parse::<u32>() else {
            continue;
        };
        let Ok(pid) = parts[1].parse::<u32>() else {
            continue;
        };
        let Ok(mb) = parts[2].parse::<u32>() else {
            continue;
        };
        if pid == 0 || mb == 0 {
            continue;
        }
        map.entry((gpu, pid))
            .and_modify(|v: &mut u32| *v = (*v).saturating_add(mb))
            .or_insert(mb);
    }
    map
}

#[cfg(any(test, windows))]
fn parse_gpu_pid_util_csv(stdout: &str) -> HashMap<(u32, u32), u8> {
    let mut map = HashMap::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.trim().split(',').map(str::trim).collect();
        if parts.len() < 3 {
            continue;
        }
        let Ok(gpu) = parts[0].parse::<u32>() else {
            continue;
        };
        let Ok(pid) = parts[1].parse::<u32>() else {
            continue;
        };
        let Ok(pct) = parts[2].parse::<u32>() else {
            continue;
        };
        if pid == 0 {
            continue;
        }
        let pct = pct.min(100) as u8;
        map.entry((gpu, pid))
            .and_modify(|v: &mut u8| *v = (*v).max(pct))
            .or_insert(pct);
    }
    map
}

// ─── Linux: DRM fdinfo (Intel / AMD / Nouveau; some NVIDIA) ─────────────────

#[cfg(target_os = "linux")]
fn live_linux_drm_process_vram_mb(interest_pids: &[u32]) -> HashMap<(u32, u32), u32> {
    let mut map = HashMap::new();
    let mut pdev_index: HashMap<String, u32> = HashMap::new();
    let pids = linux_pids_to_scan(interest_pids);
    for pid in pids {
        let Some((gpu, mb)) = linux_pid_drm_mem_mb(pid, &mut pdev_index) else {
            continue;
        };
        if mb == 0 {
            continue;
        }
        map.entry((gpu, pid))
            .and_modify(|v: &mut u32| *v = (*v).max(mb))
            .or_insert(mb);
    }
    map
}

#[cfg(target_os = "linux")]
fn live_linux_drm_process_util_pct(interest_pids: &[u32]) -> HashMap<(u32, u32), u8> {
    use std::sync::Mutex;
    use std::time::Instant;
    struct Prev {
        at: Instant,
        engines: HashMap<(u32, u32, String), u64>,
    }
    static PREV: Mutex<Option<Prev>> = Mutex::new(None);

    let mut now_engines: HashMap<(u32, u32, String), u64> = HashMap::new();
    let mut pdev_index: HashMap<String, u32> = HashMap::new();
    let pids = linux_pids_to_scan(interest_pids);
    for pid in pids {
        for (gpu, engine, ns) in linux_pid_drm_engines(pid, &mut pdev_index) {
            now_engines.insert((gpu, pid, engine), ns);
        }
    }

    let mut out = HashMap::new();
    let Ok(mut guard) = PREV.lock() else {
        return out;
    };
    let sample_at = Instant::now();
    if let Some(prev) = guard.as_ref() {
        let dt_ns = sample_at.duration_since(prev.at).as_nanos() as u64;
        if dt_ns > 50_000_000 {
            // Aggregate busy ns across engines per (gpu,pid), cap at wall time.
            let mut busy: HashMap<(u32, u32), u64> = HashMap::new();
            for ((gpu, pid, engine), ns) in &now_engines {
                let Some(prev_ns) = prev.engines.get(&(*gpu, *pid, engine.clone())) else {
                    continue;
                };
                let delta = ns.saturating_sub(*prev_ns);
                *busy.entry((*gpu, *pid)).or_insert(0) =
                    busy.get(&(*gpu, *pid)).copied().unwrap_or(0).saturating_add(delta);
            }
            for ((gpu, pid), busy_ns) in busy {
                let pct = ((busy_ns.min(dt_ns) as f64) * 100.0 / dt_ns as f64).round() as u32;
                if pct > 0 {
                    out.insert((gpu, pid), pct.min(100) as u8);
                }
            }
        }
    }
    *guard = Some(Prev {
        at: sample_at,
        engines: now_engines,
    });
    out
}

#[cfg(target_os = "linux")]
fn linux_pids_to_scan(interest_pids: &[u32]) -> Vec<u32> {
    if !interest_pids.is_empty() {
        return interest_pids
            .iter()
            .copied()
            .filter(|p| *p > 0)
            .collect();
    }
    // No interest list: light scan of /proc (vendor CLIs already cover most GPUs).
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.filter_map(|e| {
        let e = e.ok()?;
        let name = e.file_name();
        let s = name.to_string_lossy();
        s.parse::<u32>().ok()
    })
    .take(512)
    .collect()
}

#[cfg(target_os = "linux")]
fn linux_pid_drm_mem_mb(
    pid: u32,
    pdev_index: &mut HashMap<String, u32>,
) -> Option<(u32, u32)> {
    let fdinfo = std::fs::read_dir(format!("/proc/{pid}/fdinfo")).ok()?;
    let mut best_gpu = 0_u32;
    let mut best_kib = 0_u64;
    for ent in fdinfo.flatten() {
        let path = ent.path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !text.contains("drm-") {
            continue;
        }
        let gpu = drm_gpu_index_from_fdinfo(&text, pdev_index);
        let kib = drm_mem_kib_from_fdinfo(&text);
        if kib > best_kib {
            best_kib = kib;
            best_gpu = gpu;
        }
    }
    if best_kib == 0 {
        return None;
    }
    let mb = ((best_kib + 512) / 1024) as u32;
    if mb == 0 {
        None
    } else {
        Some((best_gpu, mb))
    }
}

#[cfg(target_os = "linux")]
fn linux_pid_drm_engines(
    pid: u32,
    pdev_index: &mut HashMap<String, u32>,
) -> Vec<(u32, String, u64)> {
    let Ok(fdinfo) = std::fs::read_dir(format!("/proc/{pid}/fdinfo")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in fdinfo.flatten() {
        let Ok(text) = std::fs::read_to_string(ent.path()) else {
            continue;
        };
        if !text.contains("drm-engine-") {
            continue;
        }
        let gpu = drm_gpu_index_from_fdinfo(&text, pdev_index);
        for line in text.lines() {
            let Some(rest) = line.strip_prefix("drm-engine-") else {
                continue;
            };
            let mut parts = rest.split_whitespace();
            let Some(name) = parts.next() else {
                continue;
            };
            // "render: 12345 ns" or "render:\t12345 ns"
            let name = name.trim_end_matches(':');
            let Some(ns_s) = parts.next() else {
                continue;
            };
            let Ok(ns) = ns_s.parse::<u64>() else {
                continue;
            };
            out.push((gpu, name.to_string(), ns));
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn drm_gpu_index_from_fdinfo(text: &str, pdev_index: &mut HashMap<String, u32>) -> u32 {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("drm-pdev:") else {
            continue;
        };
        let pdev = rest.trim().to_string();
        if pdev.is_empty() {
            continue;
        }
        if let Some(idx) = pdev_index.get(&pdev) {
            return *idx;
        }
        let idx = pdev_index.len() as u32;
        pdev_index.insert(pdev, idx);
        return idx;
    }
    0
}

#[cfg(target_os = "linux")]
fn drm_mem_kib_from_fdinfo(text: &str) -> u64 {
    // Prefer dedicated VRAM; fall back to system/shared for iGPU unified memory.
    let mut vram = 0_u64;
    let mut fallback = 0_u64;
    for line in text.lines() {
        let (key, rest) = match line.split_once(':') {
            Some(pair) => pair,
            None => continue,
        };
        let kib = parse_drm_size_kib(rest.trim());
        if kib == 0 {
            continue;
        }
        match key.trim() {
            "drm-memory-vram" => vram = vram.max(kib),
            "drm-memory-system" | "drm-total-memory" | "drm-shared-system" | "drm-memory-gtt" => {
                fallback = fallback.max(kib);
            }
            _ => {}
        }
    }
    if vram > 0 {
        vram
    } else {
        fallback
    }
}

#[cfg(any(test, target_os = "linux"))]
fn parse_drm_size_kib(raw: &str) -> u64 {
    let mut parts = raw.split_whitespace();
    let Some(n_s) = parts.next() else {
        return 0;
    };
    let Ok(n) = n_s.parse::<u64>() else {
        return 0;
    };
    let unit = parts.next().unwrap_or("KiB").to_ascii_lowercase();
    if unit.starts_with("mi") {
        n.saturating_mul(1024)
    } else if unit.starts_with("gi") {
        n.saturating_mul(1024 * 1024)
    } else {
        // KiB / bytes labeled as KiB in drm fdinfo
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_windows_style_csv() {
        let map = parse_gpu_pid_mb_csv("0,1234,512\n1,1234,128\n0,0,99\nbad\n");
        assert_eq!(map.get(&(0, 1234)), Some(&512));
        assert_eq!(map.get(&(1, 1234)), Some(&128));
        assert!(!map.contains_key(&(0, 0)));
    }

    #[test]
    fn parse_util_keeps_max() {
        let map = parse_gpu_pid_util_csv("0,9,10\n0,9,40\n0,9,200\n");
        assert_eq!(map.get(&(0, 9)), Some(&100));
    }

    #[test]
    fn merge_fill_does_not_overwrite() {
        let mut dst = HashMap::from([((0, 1), 100_u32)]);
        merge_fill_u32(
            &mut dst,
            HashMap::from([((0, 1), 50), ((0, 2), 75)]),
        );
        assert_eq!(dst.get(&(0, 1)), Some(&100));
        assert_eq!(dst.get(&(0, 2)), Some(&75));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn drm_size_units() {
        assert_eq!(parse_drm_size_kib("2048 KiB"), 2048);
        assert_eq!(parse_drm_size_kib("2 MiB"), 2048);
    }
}
