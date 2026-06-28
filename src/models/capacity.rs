use crate::compute_pool::{PoolStrategy, VirtualCard};
use crate::protocol::CatalogModel;

/// Whether this machine can download and serve a catalog model on its virtual compute card.
pub fn can_host_model(model: &CatalogModel, card: &VirtualCard, ram_gb: u32) -> bool {
    let min_vram = model.min_vram_gb.unwrap_or(0);
    let min_ram = model.min_ram_gb.unwrap_or(0);
    let weight_gb = model.weight_size_gb.unwrap_or(0);

    // Fits entirely on GPU VRAM.
    if min_vram > 0 && card.total_vram_gb >= min_vram {
        return ram_gb >= min_ram;
    }

    // Laptop / partial VRAM: offload weights to system RAM (mmap + CPU layers).
    if !card.cuda_device_ids.is_empty()
        && card.total_vram_gb >= 4
        && matches!(
            card.strategy,
            PoolStrategy::GpuWithCpuOffload | PoolStrategy::Single
        )
    {
        let ram_needed = weight_gb.saturating_add(8).max(min_ram);
        return ram_gb >= ram_needed;
    }

    // CPU-only inference.
    if card.strategy == PoolStrategy::CpuOnly {
        return ram_gb >= weight_gb.saturating_add(8).max(min_ram);
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute_pool::PoolDevice;

    fn laptop_card() -> VirtualCard {
        VirtualCard {
            devices: vec![PoolDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "RTX 3050 Ti".into(),
                vram_gb: 4,
                cuda_index: Some(0),
            }],
            strategy: PoolStrategy::GpuWithCpuOffload,
            display_name: "RTX 3050 Ti".into(),
            total_vram_gb: 4,
            tensor_split: Vec::new(),
            cuda_device_ids: vec![0],
            gpu_layer_budget: 13,
        }
    }

    #[test]
    fn qwen_fits_on_laptop_with_offload() {
        let model = CatalogModel {
            model_id: "qwen-3.6".into(),
            display_name: String::new(),
            runtime_model: String::new(),
            max_context_tokens: 0,
            regions: Vec::new(),
            weight_size_gb: Some(5),
            min_vram_gb: Some(4),
            min_ram_gb: Some(10),
            weights: None,
        };
        assert!(can_host_model(&model, &laptop_card(), 16));
        assert!(!can_host_model(&model, &laptop_card(), 7));
    }
}
