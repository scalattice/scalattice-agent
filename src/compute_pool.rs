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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolStrategy {
    Single,
    TensorParallel,
    GpuWithCpuOffload,
    CpuOnly,
}

#[derive(Debug, Clone)]
pub struct VirtualCard {
    pub devices: Vec<PoolDevice>,
    pub strategy: PoolStrategy,
    pub display_name: String,
    pub total_vram_gb: u32,
    /// Fraction of model tensors per CUDA device (sums to 1.0). Empty when CPU-only.
    #[cfg_attr(not(test), allow(dead_code))]
    pub tensor_split: Vec<f32>,
    pub cuda_device_ids: Vec<u32>,
    /// Layers to keep on GPU when CPU offload is active (0 = CPU-only path).
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

    for device in enabled {
        let vram_gb = device.vram_gb.unwrap_or(0).max(1);
        let cuda_index = parse_cuda_index(&device.id);
        if device.kind == "discrete" && cuda_index.is_some() {
            cuda_ids.push(cuda_index.unwrap());
            discrete_vram.push(vram_gb);
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
    let display_name = if pool_devices.len() == 1 {
        pool_devices[0].name.clone()
    } else {
        format!(
            "Virtual {} ({} devices)",
            format_vram(total_vram_gb),
            pool_devices.len()
        )
    };

    let total_discrete_vram: u32 = discrete_vram.iter().copied().sum();

    let strategy = if cuda_ids.is_empty() {
        PoolStrategy::CpuOnly
    } else if cuda_ids.len() > 1 && total_discrete_vram >= 48 {
        PoolStrategy::TensorParallel
    } else if total_discrete_vram < 48 {
        // Laptops / single-GPU hosts: offload layers to CPU RAM instead of OOM on load.
        PoolStrategy::GpuWithCpuOffload
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

    let gpu_vram = discrete_vram.iter().copied().sum::<u32>();
    let gpu_layer_budget = match strategy {
        PoolStrategy::CpuOnly => 0,
        PoolStrategy::GpuWithCpuOffload => {
            // ~1 transformer block per 300 MiB VRAM (conservative for large quant models).
            ((gpu_vram as f32 * 1024.0) / 300.0).round().clamp(1.0, 80.0) as u32
        }
        _ => 999,
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
}
