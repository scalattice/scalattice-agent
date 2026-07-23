use crate::specs::ComputeDevice;
use anyhow::{bail, Result};

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
    // Multi-GPU: only use TensorParallel when every CUDA GPU has enough VRAM.
    // On small cards (e.g. 2×4GB), llama.cpp TP + gpu-full often CUDA-abort()s the
    // whole process — cascade never runs. Demote to Single on the largest GPU.
    let (strategy, total_vram_gb, display_name, tensor_split, uses_vulkan, cuda_ids) =
        if !cuda_ids.is_empty() {
            if cuda_ids.len() > 1 && tensor_parallel_viable(&cuda_vram) {
                let total: u32 = cuda_vram.iter().copied().sum();
                let name = format!("Virtual {} ({} GPUs)", format_vram(total), cuda_names.len());
                let split = vram_proportions(&cuda_vram);
                (
                    PoolStrategy::TensorParallel,
                    total,
                    name,
                    split,
                    false,
                    cuda_ids,
                )
            } else {
                // Single GPU, or multi-GPU demoted to the largest device only.
                let (best_i, &best_vram) = cuda_vram
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, v)| *v)
                    .expect("cuda_vram non-empty");
                if cuda_ids.len() > 1 {
                    tracing::info!(
                        kept = %cuda_names[best_i],
                        kept_vram_gb = best_vram,
                        enabled_gpus = cuda_ids.len(),
                        min_vram_gb = cuda_vram.iter().copied().min().unwrap_or(0),
                        "multi-GPU tensor-parallel unsafe on small VRAM; using largest GPU only"
                    );
                }
                (
                    PoolStrategy::Single,
                    best_vram,
                    cuda_names[best_i].clone(),
                    Vec::new(),
                    false,
                    vec![cuda_ids[best_i]],
                )
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

    let gpu_layer_budget = match strategy {
        PoolStrategy::CpuOnly => 0,
        // Budget from the VRAM we will actually place on (primary / TP total).
        _ => offload_layer_budget(total_vram_gb.max(1)),
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

/// Tensor-parallel across consumer GPUs below this per-GPU VRAM routinely hard-aborts
/// inside llama.cpp CUDA (process death — no Rust cascade). Require every GPU ≥ 8GB.
pub fn tensor_parallel_viable(cuda_vram_gb: &[u32]) -> bool {
    cuda_vram_gb.len() >= 2 && cuda_vram_gb.iter().copied().min().unwrap_or(0) >= 8
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

/// CUDA index of the GPU used for offload / Single placement.
pub fn primary_cuda_device(pool: &VirtualCard) -> Option<u32> {
    // After multi-GPU demotion, cuda_device_ids is already the chosen device.
    if pool.cuda_device_ids.len() == 1 {
        return pool.cuda_device_ids.first().copied();
    }
    pool.devices
        .iter()
        .filter(|d| d.kind == "discrete" && d.cuda_index.is_some())
        .max_by_key(|d| d.vram_gb)
        .and_then(|d| d.cuda_index)
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
    fn small_multi_gpu_demotes_to_largest_single() {
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

        // 2×4GB TP abort()s in llama.cpp — use one GPU with offload cascade instead.
        assert_eq!(card.strategy, PoolStrategy::Single);
        assert_eq!(card.total_vram_gb, 4);
        assert_eq!(card.cuda_device_ids.len(), 1);
        assert!(!card.uses_vulkan);
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
    fn heterogeneous_keeps_largest_when_sibling_too_small_for_tp() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "GTX 1650".into(),
                vram_gb: Some(4),
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

        assert_eq!(card.strategy, PoolStrategy::Single);
        assert_eq!(card.total_vram_gb, 24);
        assert_eq!(card.cuda_device_ids, vec![1]);
        assert_eq!(card.display_name, "RTX 4090");
    }

    #[test]
    fn multi_gpu_is_one_tensor_parallel_pool_regardless_of_size() {
        // Kept name for git blame; size now matters — roomy cards only.
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
