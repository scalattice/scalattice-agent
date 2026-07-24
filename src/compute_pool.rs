use crate::specs::ComputeDevice;
use anyhow::{bail, Result};
use tracing::warn;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PoolDevice {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub vram_gb: u32,
    pub cuda_index: Option<u32>,
}

/// How the pool prefers to run when accelerators allow a full fit.
///
/// Partial GPU↔CPU offload is a cascade fallback (see `load_param_candidates`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolStrategy {
    /// One NVIDIA CUDA device; try all layers on GPU first.
    Single,
    /// All enabled NVIDIA CUDA devices as one accelerator (tensor split by VRAM).
    TensorParallel,
    /// Enabled AMD/Intel GPUs via llama.cpp Vulkan (no CUDA device pin).
    Vulkan,
    CpuOnly,
}

#[derive(Debug, Clone)]
pub struct VirtualCard {
    pub devices: Vec<PoolDevice>,
    pub strategy: PoolStrategy,
    pub display_name: String,
    pub total_vram_gb: u32,
    /// Fraction of model tensors per CUDA device (sums to 1.0). Empty unless CUDA TP.
    #[cfg_attr(not(test), allow(dead_code))]
    pub tensor_split: Vec<f32>,
    pub cuda_device_ids: Vec<u32>,
    /// True when strategy is Vulkan (AMD/Intel enabled, no CUDA in the pool).
    pub uses_vulkan: bool,
    /// Conservative layer count for offload *fallback* tiers after full-GPU OOM.
    pub gpu_layer_budget: u32,
}

/// Must run **before** `init_backend()`. Mixed NVIDIA gens (e.g. 1650 Super + 1050 Ti)
/// make llama.cpp CUDA abort even on "single device" loads while both cards stay visible.
/// Hide everything except the largest card from the CUDA runtime.
pub fn restrict_heterogeneous_cuda_visibility(devices: &[ComputeDevice]) {
    let mut cuda: Vec<(u32, String, u32)> = Vec::new();
    for device in devices.iter().filter(|d| d.enabled) {
        let Some(idx) = parse_cuda_index(&device.id) else {
            continue;
        };
        if device.kind != "discrete" {
            continue;
        }
        cuda.push((idx, device.name.clone(), effective_vram_gb(device)));
    }
    if cuda.len() < 2 {
        return;
    }
    let names: Vec<String> = cuda.iter().map(|c| c.1.clone()).collect();
    let vrams: Vec<u32> = cuda.iter().map(|c| c.2).collect();
    if cuda_name_vram_homogeneous(&names, &vrams) {
        return;
    }

    let primary_pos = pick_primary_cuda_pos(
        &cuda.iter().map(|c| c.0).collect::<Vec<_>>(),
        &vrams,
    );
    let (physical, name, vram) = &cuda[primary_pos];
    // SAFETY: must be set before the first CUDA / llama.cpp backend init in this process.
    std::env::set_var("CUDA_VISIBLE_DEVICES", physical.to_string());
    warn!(
        kept_cuda_index = physical,
        kept_name = %name,
        kept_vram_gb = vram,
        hidden_gpus = cuda.len() - 1,
        "mixed NVIDIA GPUs: CUDA_VISIBLE_DEVICES limited to the largest card so llama.cpp cannot abort on multi-arch init"
    );
}

fn cuda_visibility_remapped_to_zero(physical: u32) -> bool {
    let Ok(raw) = std::env::var("CUDA_VISIBLE_DEVICES") else {
        return false;
    };
    let parts: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    parts.len() == 1 && parts[0].parse::<u32>().ok() == Some(physical)
}

/// Normalize NVIDIA marketing names so "NVIDIA GeForce RTX 4090" == "rtx 4090" family match.
pub fn normalize_cuda_sku(name: &str) -> String {
    let mut s = name.trim().to_ascii_lowercase();
    for prefix in [
        "nvidia ",
        "geforce ",
        "tesla ",
        "quadro ",
        "rtx ",
        "gtx ",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim_start().to_string();
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn cuda_name_vram_homogeneous(names: &[String], vrams: &[u32]) -> bool {
    if names.len() < 2 || names.len() != vrams.len() {
        return false;
    }
    let skus: Vec<String> = names.iter().map(|n| normalize_cuda_sku(n)).collect();
    if skus.iter().any(|s| s.is_empty()) || !skus.iter().all(|s| s == &skus[0]) {
        return false;
    }
    let min_v = *vrams.iter().min().unwrap_or(&0);
    let max_v = *vrams.iter().max().unwrap_or(&0);
    max_v.saturating_sub(min_v) <= 1
}

fn pick_primary_cuda_pos(ids: &[u32], vrams: &[u32]) -> usize {
    (0..ids.len())
        .max_by(|&i, &j| {
            vrams[i]
                .cmp(&vrams[j])
                // On equal VRAM prefer the lower CUDA index (usually the primary slot).
                .then_with(|| ids[j].cmp(&ids[i]))
        })
        .unwrap_or(0)
}

pub fn build_virtual_card(devices: &[ComputeDevice]) -> Result<VirtualCard> {
    let enabled: Vec<&ComputeDevice> = devices.iter().filter(|d| d.enabled).collect();
    if enabled.is_empty() {
        bail!("no compute devices enabled");
    }

    let mut pool_devices = Vec::new();
    let mut cuda_ids = Vec::new();
    let mut cuda_vram = Vec::new();
    let mut cuda_names = Vec::new();
    let mut vulkan_vram = Vec::new();
    let mut vulkan_names = Vec::new();

    for device in &enabled {
        let vram_gb = effective_vram_gb(device);
        let cuda_index = parse_cuda_index(&device.id);

        if device.kind == "discrete" && cuda_index.is_some() {
            cuda_ids.push(cuda_index.unwrap());
            cuda_vram.push(vram_gb);
            cuda_names.push(device.name.clone());
        } else if is_vulkan_accelerator(device) {
            vulkan_vram.push(vram_gb);
            vulkan_names.push(device.name.clone());
        }

        pool_devices.push(PoolDevice {
            id: device.id.clone(),
            kind: device.kind.clone(),
            name: device.name.clone(),
            vram_gb,
            cuda_index,
        });
    }

    // Prefer CUDA when any NVIDIA device is enabled. Otherwise use Vulkan accelerators
    // (AMD discrete / Intel Arc / iGPU). CPU alone → CpuOnly.
    //
    // Matched multi-GPU → TensorParallel topology (per-model TP still gated at load).
    // Mixed SKU/VRAM → demote to Single on the largest card (and ideally CUDA_VISIBLE_DEVICES).
    let (strategy, total_vram_gb, display_name, tensor_split, uses_vulkan, cuda_ids) =
        if !cuda_ids.is_empty() {
            if cuda_ids.len() > 1 && !cuda_name_vram_homogeneous(&cuda_names, &cuda_vram) {
                let primary_pos = pick_primary_cuda_pos(&cuda_ids, &cuda_vram);
                let physical = cuda_ids[primary_pos];
                let llama_id = if cuda_visibility_remapped_to_zero(physical) {
                    0
                } else {
                    physical
                };
                let vram = cuda_vram[primary_pos];
                let name = cuda_names[primary_pos].clone();
                warn!(
                    kept = %name,
                    llama_cuda_id = llama_id,
                    physical_cuda_id = physical,
                    ignored_gpus = cuda_ids.len() - 1,
                    "mixed NVIDIA GPUs: inference pool demoted to single largest card"
                );
                (
                    PoolStrategy::Single,
                    vram,
                    name,
                    Vec::new(),
                    false,
                    vec![llama_id],
                )
            } else {
                let total: u32 = cuda_vram.iter().copied().sum();
                let name = match cuda_names.len() {
                    1 => cuda_names[0].clone(),
                    n => format!("Virtual {} ({} GPUs)", format_vram(total), n),
                };
                let strategy = if cuda_ids.len() > 1 {
                    PoolStrategy::TensorParallel
                } else {
                    PoolStrategy::Single
                };
                let split = if cuda_ids.len() > 1 {
                    vram_proportions(&cuda_vram)
                } else {
                    Vec::new()
                };
                (strategy, total, name, split, false, cuda_ids)
            }
        } else if !vulkan_names.is_empty() && vulkan_runtime_supported() {
            let total: u32 = vulkan_vram.iter().copied().sum::<u32>().max(1);
            let name = match vulkan_names.len() {
                1 => vulkan_names[0].clone(),
                n => format!("Virtual {} ({} GPUs · Vulkan)", format_vram(total), n),
            };
            (
                PoolStrategy::Vulkan,
                total,
                name,
                Vec::new(),
                true,
                Vec::new(),
            )
        } else {
            let name = if pool_devices.len() == 1 {
                pool_devices[0].name.clone()
            } else {
                format!("CPU pool ({} devices)", pool_devices.len())
            };
            (
                PoolStrategy::CpuOnly,
                0,
                name,
                Vec::new(),
                false,
                Vec::new(),
            )
        };

    // Offload budgets are for the primary GPU (largest), even on TP pools — cascade
    // offload tiers always pin a single device.
    let budget_vram = if !cuda_vram.is_empty() {
        cuda_vram.iter().copied().max().unwrap_or(1)
    } else {
        total_vram_gb.max(1)
    };
    let gpu_layer_budget = match strategy {
        PoolStrategy::CpuOnly => 0,
        _ => offload_layer_budget(budget_vram),
    };

    Ok(VirtualCard {
        devices: pool_devices,
        strategy,
        display_name,
        total_vram_gb,
        tensor_split,
        cuda_device_ids: cuda_ids,
        uses_vulkan,
        gpu_layer_budget,
    })
}

/// AMD discrete, Intel Arc, or integrated GPUs — served via Vulkan when CUDA is absent.
pub fn is_vulkan_accelerator(device: &ComputeDevice) -> bool {
    if device.kind == "cpu" {
        return false;
    }
    if parse_cuda_index(&device.id).is_some() {
        return false;
    }
    if device.kind == "integrated" {
        return true;
    }
    // Discrete non-NVIDIA (amd:*, pci amd/intel arc, etc.)
    device.kind == "discrete"
        && (device.id.starts_with("amd:")
            || device.id.starts_with("pci-amd:")
            || device.id.starts_with("pci-intel:")
            || device.name.to_ascii_lowercase().contains("amd")
            || device.name.to_ascii_lowercase().contains("radeon")
            || device.name.to_ascii_lowercase().contains("arc"))
}

fn effective_vram_gb(device: &ComputeDevice) -> u32 {
    if let Some(v) = device.vram_gb.filter(|v| *v > 0) {
        return v;
    }
    match device.kind.as_str() {
        // Soft estimate for shared iGPU memory — cascade + RAM checks are the real gate.
        "integrated" => 2,
        "discrete" => 1,
        _ => 0,
    }
}

/// Linux release builds compile Vulkan in; Windows CUDA-only builds fall back to CPU
/// for AMD/Intel until a Windows Vulkan release exists.
pub fn vulkan_runtime_supported() -> bool {
    cfg!(feature = "vulkan")
}

/// Conservative layer estimate for CPU-offload fallbacks after a full-GPU OOM.
pub fn offload_layer_budget(total_discrete_vram_gb: u32) -> u32 {
    const KV_HEADROOM_MIB: f32 = 768.0;
    let usable_mib = (total_discrete_vram_gb as f32 * 1024.0 - KV_HEADROOM_MIB).max(300.0);
    (usable_mib / 300.0).round().clamp(1.0, 80.0) as u32
}

/// CUDA index of the largest enabled NVIDIA GPU (offload / Single fallbacks).
pub fn primary_cuda_device(pool: &VirtualCard) -> Option<u32> {
    let mut best: Option<(u32, u32)> = None; // (vram, cuda_index)
    for d in pool.devices.iter().filter(|d| d.kind == "discrete") {
        let Some(idx) = d.cuda_index else {
            continue;
        };
        let vram = d.vram_gb;
        best = Some(match best {
            None => (vram, idx),
            Some((bv, bi)) => {
                if vram > bv || (vram == bv && idx < bi) {
                    (vram, idx)
                } else {
                    (bv, bi)
                }
            }
        });
    }
    // After mixed-GPU demotion the pool's llama device list is authoritative.
    if pool.strategy == PoolStrategy::Single && pool.cuda_device_ids.len() == 1 {
        return pool.cuda_device_ids.first().copied();
    }
    best.map(|(_, idx)| idx)
}

pub fn primary_cuda_vram_gb(pool: &VirtualCard) -> u32 {
    if pool.strategy == PoolStrategy::Single && pool.cuda_device_ids.len() == 1 {
        return pool.total_vram_gb.max(1);
    }
    pool.devices
        .iter()
        .filter(|d| d.kind == "discrete" && d.cuda_index.is_some())
        .map(|d| d.vram_gb)
        .max()
        .unwrap_or(0)
}

/// Per-device VRAM for CUDA ids in the pool (same order as `cuda_device_ids`).
pub fn cuda_device_vram_gb(pool: &VirtualCard) -> Vec<u32> {
    if pool.strategy == PoolStrategy::Single && pool.cuda_device_ids.len() == 1 {
        return vec![pool.total_vram_gb.max(1)];
    }
    pool.cuda_device_ids
        .iter()
        .filter_map(|id| {
            pool.devices
                .iter()
                .find(|d| d.cuda_index == Some(*id))
                .map(|d| d.vram_gb)
        })
        .collect()
}

pub fn format_vram(gb: u32) -> String {
    format!("{gb}GB")
}

fn parse_cuda_index(id: &str) -> Option<u32> {
    let rest = id.strip_prefix("nvidia:")?;
    rest.parse().ok()
}

fn vram_proportions(vram_gb: &[u32]) -> Vec<f32> {
    let total = vram_gb.iter().copied().sum::<u32>().max(1) as f32;
    vram_gb
        .iter()
        .map(|gb| *gb as f32 / total)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_amd_without_rocm_uses_vulkan() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "pci-amd:0".into(),
                kind: "discrete".into(),
                name: "AMD Radeon RX 6800".into(),
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "cpu:0".into(),
                kind: "cpu".into(),
                name: "CPU".into(),
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();

        if vulkan_runtime_supported() {
            assert_eq!(card.strategy, PoolStrategy::Vulkan);
            assert!(card.uses_vulkan);
            assert_eq!(card.total_vram_gb, 1); // soft estimate without VRAM probe
        } else {
            assert_eq!(card.strategy, PoolStrategy::CpuOnly);
        }
    }

    #[test]
    fn mixed_multi_gpu_demotes_to_single_largest() {
        // Chillblast-like: placement must not keep TensorParallel topology.
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "GTX 1650 SUPER".into(),
                vram_gb: Some(4),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "nvidia:1".into(),
                kind: "discrete".into(),
                name: "GTX 1050 Ti".into(),
                vram_gb: Some(4),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "cpu:0".into(),
                kind: "cpu".into(),
                name: "CPU".into(),
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();

        assert_eq!(card.strategy, PoolStrategy::Single);
        assert_eq!(card.total_vram_gb, 4);
        assert_eq!(card.cuda_device_ids, vec![0]);
        assert_eq!(card.display_name, "GTX 1650 SUPER");
        assert_eq!(card.gpu_layer_budget, offload_layer_budget(4));
    }

    #[test]
    fn roomy_multi_gpu_uses_tensor_parallel() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "RTX 4090".into(),
                vram_gb: Some(24),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "nvidia:1".into(),
                kind: "discrete".into(),
                name: "RTX 4090".into(),
                vram_gb: Some(24),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();

        assert_eq!(card.strategy, PoolStrategy::TensorParallel);
        assert_eq!(card.total_vram_gb, 48);
        assert_eq!(card.cuda_device_ids, vec![0, 1]);
        assert_eq!(card.display_name, "Virtual 48GB (2 GPUs)");
    }

    #[test]
    fn multi_gpu_is_one_tensor_parallel_pool() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "A".into(),
                vram_gb: Some(16),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "nvidia:1".into(),
                kind: "discrete".into(),
                name: "B".into(),
                vram_gb: Some(16),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();

        assert_eq!(card.strategy, PoolStrategy::TensorParallel);
        assert_eq!(card.total_vram_gb, 32);
        assert!(!card.uses_vulkan);
        assert_eq!(card.display_name, "Virtual 32GB (2 GPUs)");
    }

    #[test]
    fn amd_only_uses_vulkan_when_feature_enabled() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "amd:0".into(),
                kind: "discrete".into(),
                name: "AMD Radeon RX 6800".into(),
                vram_gb: Some(16),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "cpu:0".into(),
                kind: "cpu".into(),
                name: "CPU".into(),
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();

        if vulkan_runtime_supported() {
            assert_eq!(card.strategy, PoolStrategy::Vulkan);
            assert!(card.uses_vulkan);
            assert!(card.cuda_device_ids.is_empty());
            assert_eq!(card.total_vram_gb, 16);
            assert_eq!(card.display_name, "AMD Radeon RX 6800");
            assert!(card.gpu_layer_budget > 0);
        } else {
            // Windows CUDA-only builds: AMD alone → CPU until Vulkan ships there.
            assert_eq!(card.strategy, PoolStrategy::CpuOnly);
            assert!(!card.uses_vulkan);
        }
    }

    #[test]
    fn integrated_only_uses_vulkan_when_feature_enabled() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "pci:0".into(),
                kind: "integrated".into(),
                name: "Intel UHD Graphics".into(),
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();

        if vulkan_runtime_supported() {
            assert_eq!(card.strategy, PoolStrategy::Vulkan);
            assert!(card.uses_vulkan);
            assert_eq!(card.total_vram_gb, 2); // soft estimate
        } else {
            assert_eq!(card.strategy, PoolStrategy::CpuOnly);
        }
    }

    #[test]
    fn nvidia_preferred_over_amd_when_both_enabled() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "RTX 5050".into(),
                vram_gb: Some(8),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "amd:0".into(),
                kind: "discrete".into(),
                name: "AMD Radeon".into(),
                vram_gb: Some(8),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();

        assert_eq!(card.strategy, PoolStrategy::Single);
        assert!(!card.uses_vulkan);
        assert_eq!(card.cuda_device_ids, vec![0]);
        assert_eq!(card.display_name, "RTX 5050");
    }

    #[test]
    fn single_gpu_prefers_full_gpu_strategy() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "RTX 5050".into(),
                vram_gb: Some(8),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "cpu:0".into(),
                kind: "cpu".into(),
                name: "CPU".into(),
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();

        assert_eq!(card.strategy, PoolStrategy::Single);
        assert_eq!(card.gpu_layer_budget, 25);
        assert_eq!(primary_cuda_device(&card), Some(0));
    }

    #[test]
    fn primary_cuda_picks_largest_gpu() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "Small".into(),
                vram_gb: Some(4),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "nvidia:1".into(),
                kind: "discrete".into(),
                name: "Large".into(),
                vram_gb: Some(16),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();
        assert_eq!(primary_cuda_device(&card), Some(1));
    }
}
