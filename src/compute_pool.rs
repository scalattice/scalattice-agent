use crate::specs::ComputeDevice;
use anyhow::{bail, Result};

#[derive(Debug, Clone)]
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
    let mut has_cpu = false;
    let mut has_integrated = false;

    for device in enabled {
        let vram_gb = device.vram_gb.unwrap_or(0).max(1);
        let cuda_index = parse_cuda_index(&device.id);
        if device.kind == "discrete" && cuda_index.is_some() {
            cuda_ids.push(cuda_index.unwrap());
            discrete_vram.push(vram_gb);
        }
        if device.kind == "cpu" {
            has_cpu = true;
        }
        if device.kind == "integrated" {
            has_integrated = true;
        }

        pool_devices.push(PoolDevice {
            id: device.id.clone(),
            kind: device.kind.clone(),
            name: device.name.clone(),
            vram_gb,
            cuda_index,
        });
    }

    let total_vram_gb: u32 = pool_devices.iter().map(|d| d.vram_gb).sum();
    let display_name = if pool_devices.len() == 1 {
        pool_devices[0].name.clone()
    } else {
        format!(
            "Virtual {} ({} devices)",
            format_vram(total_vram_gb),
            pool_devices.len()
        )
    };

    let strategy = if cuda_ids.len() > 1 {
        PoolStrategy::TensorParallel
    } else if cuda_ids.len() == 1 && (has_cpu || has_integrated) {
        PoolStrategy::GpuWithCpuOffload
    } else if cuda_ids.is_empty() {
        PoolStrategy::CpuOnly
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
            // Bias most layers to GPU, leave headroom for CPU RAM on old laptops.
            let ratio = gpu_vram as f32 / total_vram_gb.max(1) as f32;
            (ratio * 80.0).round().clamp(8.0, 80.0) as u32
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

pub fn format_tensor_split(split: &[f32]) -> String {
    split
        .iter()
        .map(|value| format!("{:.3}", value))
        .collect::<Vec<_>>()
        .join(",")
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
