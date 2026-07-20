use crate::compute_pool::{PoolStrategy, VirtualCard};
use crate::protocol::CatalogModel;

fn gb_ceil(v: Option<f64>) -> u32 {
    let n = v.unwrap_or(0.0);
    if n <= 0.0 {
        return 0;
    }
    n.ceil().min(u32::MAX as f64) as u32
}

/// Whether this machine can download and serve a catalog model on its virtual compute card.
pub fn can_host_model(model: &CatalogModel, card: &VirtualCard, ram_gb: u32) -> bool {
    let min_vram = gb_ceil(model.min_vram_gb);
    let min_ram = gb_ceil(model.min_ram_gb);
    let weight_gb = gb_ceil(model.weight_size_gb);

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
