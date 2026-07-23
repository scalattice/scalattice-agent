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

/// How the pool prefers to run when VRAM allows a full fit.
///
/// Partial GPU↔CPU offload is not a primary strategy — it is a cascade fallback
/// when full placement OOMs (see `load_param_candidates` in `llm::embedded`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolStrategy {
    /// One CUDA device; try all layers on GPU first.
    Single,
    /// All enabled CUDA devices as one accelerator (tensor split by VRAM share).
    TensorParallel,
    CpuOnly,
}

#[derive(Debug, Clone)]
pub struct VirtualCard {
    pub devices: Vec<PoolDevice>,
    pub strategy: PoolStrategy,
    pub display_name: String,
    pub total_vram_gb: u32,
    /// Fraction of model tensors per CUDA device (sums to 1.0). Empty when CPU-only / single.
    #[cfg_attr(not(test), allow(dead_code))]
    pub tensor_split: Vec<f32>,
    pub cuda_device_ids: Vec<u32>,
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
    let mut discrete_vram = Vec::new();
    let mut discrete_names = Vec::new();

    for device in enabled {
        let vram_gb = device.vram_gb.unwrap_or(0).max(1);
        let cuda_index = parse_cuda_index(&device.id);
        if device.kind == "discrete" && cuda_index.is_some() {
            cuda_ids.push(cuda_index.unwrap());
            discrete_vram.push(vram_gb);
            discrete_names.push(device.name.clone());
        }

        pool_devices.push(PoolDevice {
            id: device.id.clone(),
            kind: device.kind.clone(),
            name: device.name.clone(),
            vram_gb,
            cuda_index,
        });
    }

    let total_vram_gb: u32 = if !discrete_vram.is_empty() {
        discrete_vram.iter().copied().sum()
    } else {
        pool_devices
            .iter()
            .filter(|d| d.kind != "cpu")
            .map(|d| d.vram_gb)
            .sum()
    };

    // Present enabled GPUs as one compute unit (CPU is RAM backing, not a peer accelerator).
    let display_name = match discrete_names.len() {
        0 if pool_devices.len() == 1 => pool_devices[0].name.clone(),
        0 => format!("Virtual {} ({} devices)", format_vram(total_vram_gb), pool_devices.len()),
        1 => discrete_names[0].clone(),
        n => format!("Virtual {} ({} GPUs)", format_vram(total_vram_gb), n),
    };

    let strategy = if cuda_ids.is_empty() {
        PoolStrategy::CpuOnly
    } else if cuda_ids.len() > 1 {
        PoolStrategy::TensorParallel
    } else {
        PoolStrategy::Single
    };

    let tensor_split = if cuda_ids.len() > 1 {
        vram_proportions(&discrete_vram)
    } else {
        Vec::new()
    };

    // Fallback offload budget from *pooled* discrete VRAM (not size-gated strategies).
    let gpu_layer_budget = if cuda_ids.is_empty() {
        0
    } else {
        offload_layer_budget(total_vram_gb)
    };

    Ok(VirtualCard {
        devices: pool_devices,
        strategy,
        display_name,
        total_vram_gb,
        tensor_split,
        cuda_device_ids: cuda_ids,
        gpu_layer_budget,
    })
}

/// Conservative layer estimate for CPU-offload fallbacks after a full-GPU OOM.
///
/// Leaves ~768 MiB for KV/compute at n_ctx=4096, then ~1 block per 300 MiB.
pub fn offload_layer_budget(total_discrete_vram_gb: u32) -> u32 {
    const KV_HEADROOM_MIB: f32 = 768.0;
    let usable_mib = (total_discrete_vram_gb as f32 * 1024.0 - KV_HEADROOM_MIB).max(300.0);
    (usable_mib / 300.0).round().clamp(1.0, 80.0) as u32
}

/// CUDA index of the largest enabled discrete GPU (offload fallbacks use this device).
pub fn primary_cuda_device(pool: &VirtualCard) -> Option<u32> {
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
    fn multi_gpu_is_one_tensor_parallel_pool_regardless_of_size() {
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

        assert_eq!(card.strategy, PoolStrategy::TensorParallel);
        assert_eq!(card.total_vram_gb, 8);
        assert_eq!(card.tensor_split.len(), 2);
        assert_eq!(card.display_name, "Virtual 8GB (2 GPUs)");
        assert_eq!(card.gpu_layer_budget, offload_layer_budget(8));
    }

    #[test]
    fn tensor_split_follows_vram() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "GPU A".into(),
                vram_gb: Some(8),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "nvidia:1".into(),
                kind: "discrete".into(),
                name: "GPU B".into(),
                vram_gb: Some(16),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();

        assert_eq!(card.strategy, PoolStrategy::TensorParallel);
        assert_eq!(card.tensor_split.len(), 2);
        assert!((card.tensor_split[0] - 0.333).abs() < 0.02);
        assert!((card.tensor_split[1] - 0.667).abs() < 0.02);
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
        assert_eq!(card.display_name, "RTX 5050");
        // (8*1024 - 768) / 300 ≈ 24.7 → 25 — used only if full GPU OOMs.
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
