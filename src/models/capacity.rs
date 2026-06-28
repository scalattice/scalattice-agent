use crate::compute_pool::{PoolStrategy, VirtualCard};
use crate::protocol::CatalogModel;

/// Whether this machine can download and serve a catalog model on its virtual compute card.
pub fn can_host_model(model: &CatalogModel, card: &VirtualCard, ram_gb: u32) -> bool {
    let min_vram = model.min_vram_gb.unwrap_or(0);
    let min_ram = model.min_ram_gb.unwrap_or(0);
    let weight_gb = model.weight_size_gb.unwrap_or(0);

    if min_ram > 0 && ram_gb < min_ram {
        return false;
    }

    if min_vram == 0 {
        return true;
    }

    if card.total_vram_gb >= min_vram {
        return true;
    }

    // CPU offload path: still needs some GPU VRAM and enough RAM for weights.
    if card.strategy == PoolStrategy::GpuWithCpuOffload && card.total_vram_gb >= 4 {
        let ram_needed = min_ram.max(weight_gb.saturating_add(8));
        return ram_gb >= ram_needed;
    }

    false
}
