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
        return ram_gb >= ram_needed;
    }

    // CPU-only inference.
    if card.strategy == PoolStrategy::CpuOnly {
        return ram_gb >= ram_needed;
    }

    false
}
