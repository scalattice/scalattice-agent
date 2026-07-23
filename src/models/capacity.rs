use crate::compute_pool::{PoolStrategy, VirtualCard};
use crate::protocol::CatalogModel;

fn gb_ceil(v: Option<f64>) -> u32 {
    let n = v.unwrap_or(0.0);
    if n <= 0.0 {
        return 0;
    }
    n.ceil().min(u32::MAX as f64) as u32
}

/// Default when an older server omits `cpuRamHeadroomGb` on ready.
pub const DEFAULT_CPU_RAM_HEADROOM_GB: u32 = 2;

/// Whether this machine can download and serve a catalog model on its virtual compute card.
/// `cpu_ram_headroom_gb` comes from the server (`ready.cpuRamHeadroomGb`).
pub fn can_host_model(
    model: &CatalogModel,
    card: &VirtualCard,
    ram_gb: u32,
    cpu_ram_headroom_gb: u32,
) -> bool {
    let min_vram = gb_ceil(model.min_vram_gb);
    let min_ram = gb_ceil(model.min_ram_gb);
    let weight_gb = gb_ceil(model.weight_size_gb);
    let ram_needed = weight_gb.saturating_add(cpu_ram_headroom_gb).max(min_ram);

    // Fits entirely on pooled accelerator VRAM (CUDA and/or Vulkan estimate).
    if min_vram > 0 && card.total_vram_gb >= min_vram {
        return ram_gb >= min_ram;
    }

    // Partial accelerator VRAM: full-GPU may OOM, but offload cascade can still serve.
    let has_accelerator = matches!(
        card.strategy,
        PoolStrategy::Single | PoolStrategy::TensorParallel | PoolStrategy::Vulkan
    );
    if has_accelerator && (card.total_vram_gb >= 4 || card.uses_vulkan) {
        return ram_gb >= ram_needed;
    }

    // CPU-only inference.
    if card.strategy == PoolStrategy::CpuOnly {
        return ram_gb >= ram_needed;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute_pool::{build_virtual_card, vulkan_runtime_supported};
    use crate::specs::ComputeDevice;

    fn catalog(min_vram: f64, weight: f64, min_ram: f64) -> CatalogModel {
        CatalogModel {
            model_id: "qwen-3-8b".into(),
            display_name: "Qwen3 8B".into(),
            runtime_model: "Qwen/Qwen3-8B".into(),
            max_context_tokens: 4096,
            regions: vec![],
            weight_size_gb: Some(weight),
            min_vram_gb: Some(min_vram),
            min_ram_gb: Some(min_ram),
            weights: None,
        }
    }

    #[test]
    fn vulkan_amd_can_host_with_enough_ram() {
        if !vulkan_runtime_supported() {
            return;
        }
        let card = build_virtual_card(&[ComputeDevice {
            id: "amd:0".into(),
            kind: "discrete".into(),
            name: "AMD Radeon".into(),
            vram_gb: Some(8),
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        }])
        .unwrap();
        assert!(can_host_model(&catalog(4.0, 5.0, 8.0), &card, 32, 2));
    }

    #[test]
    fn cpu_only_hosts_when_ram_allows() {
        let card = build_virtual_card(&[ComputeDevice {
            id: "cpu:0".into(),
            kind: "cpu".into(),
            name: "CPU".into(),
            vram_gb: None,
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        }])
        .unwrap();
        assert!(can_host_model(&catalog(4.0, 5.0, 8.0), &card, 16, 2));
        assert!(!can_host_model(&catalog(4.0, 5.0, 8.0), &card, 4, 2));
    }
}
