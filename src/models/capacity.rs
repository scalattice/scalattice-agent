use crate::compute_pool::{
    build_compute_slots, build_tp_card_for_group, build_virtual_card, PoolStrategy, VirtualCard,
};
use crate::protocol::CatalogModel;
use crate::specs::ComputeDevice;

fn gb_ceil(v: Option<f64>) -> u32 {
    let n = v.unwrap_or(0.0);
    if n <= 0.0 {
        return 0;
    }
    n.ceil().min(u32::MAX as f64) as u32
}

/// Platform default for CPU/offload RAM headroom (GB). Matches server
/// `DEFAULT_CPU_RAM_HEADROOM_GB`; used only until `ready` sets the live value.
pub const DEFAULT_CPU_RAM_HEADROOM_GB: u32 = 2;

fn model_is_vision(model: &CatalogModel) -> bool {
    model.vision_model
}

pub fn hosting_min_vram_gb(model: &CatalogModel) -> u32 {
    gb_ceil(model.min_vram_gb)
}

/// KV/compute overhead reserved on top of GGUF weight for a GPU-full placement.
/// Prefer [`gpu_full_host_need_gb`] — this is only the unknown-shape catalog extra
/// at 4k for an ~8B Q4 (kept as a name so older comments still grep).
#[allow(dead_code)]
pub const GPU_FULL_HEADROOM_GB: f64 = 2.0;

/// VRAM a slot must have free to count as a **full** GPU host.
#[cfg_attr(not(test), allow(dead_code))]
pub fn gpu_full_host_need_gb(model: &CatalogModel) -> f64 {
    gpu_full_host_need_gb_for_job(model, false)
}

pub fn gpu_full_host_need_gb_for_job(model: &CatalogModel, need_vision: bool) -> f64 {
    let n_ctx = super::vram_plan::job_n_ctx(model, need_vision);
    let weight = model
        .weight_size_gb
        .filter(|w| *w > 0.05)
        .or_else(|| {
            super::storage::resolve_model_gguf(&model.runtime_model).and_then(|path| {
                std::fs::metadata(path)
                    .ok()
                    .map(|m| m.len() as f64 / (1024.0 * 1024.0 * 1024.0))
            })
        })
        .unwrap_or(0.0);
    let shape = super::storage::resolve_model_gguf(&model.runtime_model)
        .and_then(|path| super::gguf_arch::gguf_shape(&path));
    let mut need = super::vram_plan::full_host_need_gb(weight, shape, n_ctx);
    if need_vision {
        if let Some(mm) = model.mmproj_size_gb.filter(|v| *v > 0.0) {
            need += mm;
        }
    }
    let catalog_floor = if need_vision {
        f64::from(image_job_min_vram_gb(model))
    } else {
        f64::from(hosting_min_vram_gb(model))
    };
    need.max(catalog_floor)
}

/// True when `available_gb` (live free, else advertised) can take weights + KV
/// without CPU offload. Float slop only — not a 50 MB “maybe it fits” gift.
pub fn vram_can_gpu_full(
    available_gb: f64,
    model: &CatalogModel,
    catalog_min_vram: u32,
    need_vision: bool,
) -> bool {
    if catalog_min_vram > 0 && available_gb + 0.005 < f64::from(catalog_min_vram) {
        return false;
    }
    available_gb + 0.005 >= gpu_full_host_need_gb_for_job(model, need_vision)
}

/// Nameplate helper for tests and callers that only have advertised GB.
#[cfg_attr(not(test), allow(dead_code))]
pub fn advertised_vram_can_gpu_full(
    advertised_vram_gb: u32,
    model: &CatalogModel,
    catalog_min_vram: u32,
) -> bool {
    vram_can_gpu_full(
        f64::from(advertised_vram_gb),
        model,
        catalog_min_vram,
        false,
    )
}

/// GPU floor for image jobs — catalog `minVramGbVision` from the server.
pub fn image_job_min_vram_gb(model: &CatalogModel) -> u32 {
    if !model_is_vision(model) {
        return hosting_min_vram_gb(model);
    }
    let vision = gb_ceil(model.min_vram_gb_vision);
    if vision > 0 {
        return vision;
    }
    hosting_min_vram_gb(model)
}

#[allow(dead_code)]
pub fn resolve_min_vram_gb_vision(model: &CatalogModel) -> u32 {
    image_job_min_vram_gb(model)
}

pub fn can_serve_vision_on_card(model: &CatalogModel, card: &VirtualCard) -> bool {
    let need = image_job_min_vram_gb(model);
    if need == 0 {
        return true;
    }
    card.total_vram_gb >= need
        && !matches!(card.strategy, PoolStrategy::CpuOnly)
}

/// True if any independent slot or homogeneous TP group can run image jobs for this model.
pub fn can_serve_vision_on_machine(
    model: &CatalogModel,
    devices: &[ComputeDevice],
    _ram_gb: u32,
) -> bool {
    if !model_is_vision(model) {
        return true;
    }
    let Ok(plan) = build_compute_slots(devices) else {
        return build_virtual_card(devices)
            .map(|card| can_serve_vision_on_card(model, &card))
            .unwrap_or(false);
    };
    if plan
        .slots
        .iter()
        .any(|slot| can_serve_vision_on_card(model, &slot.card))
    {
        return true;
    }
    for phys in plan.tp_groups.values() {
        if let Ok(tp) = build_tp_card_for_group(devices, phys) {
            if can_serve_vision_on_card(model, &tp) {
                return true;
            }
        }
    }
    false
}

/// Whether this machine can download and serve a catalog model on its virtual compute card.
/// `cpu_ram_headroom_gb` comes from the server (`ready.cpuRamHeadroomGb`).
pub fn can_host_model(
    model: &CatalogModel,
    card: &VirtualCard,
    ram_gb: u32,
    cpu_ram_headroom_gb: u32,
) -> bool {
    let min_vram = hosting_min_vram_gb(model);
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
        PoolStrategy::Single | PoolStrategy::TensorParallel | PoolStrategy::Vulkan | PoolStrategy::Metal
    );
    if has_accelerator && (card.total_vram_gb >= 4 || card.uses_vulkan || card.strategy == PoolStrategy::Metal) {
        return ram_gb >= ram_needed;
    }

    // CPU-only inference.
    if card.strategy == PoolStrategy::CpuOnly {
        return ram_gb >= ram_needed;
    }

    false
}

/// True if any independent slot or homogeneous TP group can host the model.
pub fn can_host_on_machine(
    model: &CatalogModel,
    devices: &[ComputeDevice],
    ram_gb: u32,
    cpu_ram_headroom_gb: u32,
) -> bool {
    let Ok(plan) = build_compute_slots(devices) else {
        return build_virtual_card(devices)
            .map(|card| can_host_model(model, &card, ram_gb, cpu_ram_headroom_gb))
            .unwrap_or(false);
    };
    if plan
        .slots
        .iter()
        .any(|slot| can_host_model(model, &slot.card, ram_gb, cpu_ram_headroom_gb))
    {
        return true;
    }
    for phys in plan.tp_groups.values() {
        if let Ok(tp) = build_tp_card_for_group(devices, phys) {
            if can_host_model(model, &tp, ram_gb, cpu_ram_headroom_gb) {
                return true;
            }
        }
    }
    false
}

/// Best card for weight download sizing: largest single slot, else homogeneous TP pool.
pub fn preferred_download_card(devices: &[ComputeDevice]) -> anyhow::Result<VirtualCard> {
    let plan = build_compute_slots(devices)?;
    let mut best = plan
        .slots
        .iter()
        .filter(|s| s.kind != "cpu")
        .max_by_key(|s| s.card.total_vram_gb)
        .map(|s| s.card.clone());
    for phys in plan.tp_groups.values() {
        if let Ok(tp) = build_tp_card_for_group(devices, phys) {
            let take = best
                .as_ref()
                .map(|card| tp.total_vram_gb > card.total_vram_gb)
                .unwrap_or(true);
            if take {
                best = Some(tp);
            }
        }
    }
    best.or_else(|| plan.slots.first().map(|s| s.card.clone()))
        .ok_or_else(|| anyhow::anyhow!("no compute slots"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute_pool::vulkan_runtime_supported;

    fn catalog(min_vram: f64, weight: f64, min_ram: f64) -> CatalogModel {
        CatalogModel {
            model_id: "qwen-3-8b".into(),
            display_name: "Qwen3 8B".into(),
            runtime_model: "Qwen/Qwen3-8B".into(),
            max_context_tokens: 4096,
            regions: vec![],
            weight_size_gb: Some(weight),
            min_vram_gb: Some(min_vram),
            min_vram_gb_vision: None,
            vision_model: false,
            text_sibling_model_id: None,
            min_ram_gb: Some(min_ram),
            mmproj_size_gb: None,
            vision_max_images: None,
            vision_max_image_side_px: None,
            vision_max_image_pixels: None,
            weights: None,
        }
    }

    fn vl_catalog(min_vram: f64, vision_vram: f64, weight: f64, min_ram: f64) -> CatalogModel {
        CatalogModel {
            model_id: "qwen-3-vl-8b".into(),
            display_name: "Qwen3 VL 8B".into(),
            runtime_model: "Qwen/Qwen3-VL-8B".into(),
            max_context_tokens: 8192,
            regions: vec![],
            weight_size_gb: Some(weight),
            min_vram_gb: Some(min_vram),
            min_vram_gb_vision: Some(vision_vram),
            vision_model: true,
            text_sibling_model_id: Some("qwen-3-8b".into()),
            min_ram_gb: Some(min_ram),
            mmproj_size_gb: None,
            vision_max_images: None,
            vision_max_image_side_px: None,
            vision_max_image_pixels: None,
            weights: None,
        }
    }

    #[test]
    fn gpu_full_need_uses_plan_not_catalog_floor() {
        let qwen = catalog(4.0, 4.68, 8.0);
        let need = gpu_full_host_need_gb(&qwen);
        assert!(need > 6.0, "{need}");
        assert!(need < 8.0, "{need}");
        assert!(!advertised_vram_can_gpu_full(6, &qwen, 4));
        assert!(advertised_vram_can_gpu_full(10, &qwen, 4));
        let eight_gb_ok = catalog(8.0, 5.0, 8.0);
        assert!(advertised_vram_can_gpu_full(8, &eight_gb_ok, 8));
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

    #[test]
    fn mixed_gpus_either_slot_can_host_small_model() {
        let devices = [
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
        ];
        assert!(can_host_on_machine(
            &catalog(4.0, 1.2, 4.0),
            &devices,
            32,
            2
        ));
    }

    #[test]
    fn dual_2gb_tp_uses_pooled_card_for_download_and_ram_offload() {
        let devices = [
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "NVIDIA T400".into(),
                vram_gb: Some(2),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "nvidia:1".into(),
                kind: "discrete".into(),
                name: "NVIDIA T400".into(),
                vram_gb: Some(2),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ];
        let card = preferred_download_card(&devices).unwrap();
        assert_eq!(card.total_vram_gb, 4);
        // 12 GB catalog minVram still hosts via TP + system RAM offload.
        assert!(can_host_model(&catalog(12.0, 11.7, 16.0), &card, 16, 2));
        assert!(can_host_on_machine(
            &catalog(12.0, 11.7, 16.0),
            &devices,
            16,
            2
        ));
    }

    #[test]
    fn vl_text_can_offload_but_images_need_vision_vram() {
        let card = build_virtual_card(&[ComputeDevice {
            id: "nvidia:0".into(),
            kind: "discrete".into(),
            name: "GTX 1650 SUPER".into(),
            vram_gb: Some(4),
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        }])
        .unwrap();
        assert!(can_host_model(
            &vl_catalog(8.0, 12.0, 4.7, 8.0),
            &card,
            32,
            2
        ));
        assert!(!can_serve_vision_on_card(
            &vl_catalog(8.0, 12.0, 4.7, 8.0),
            &card
        ));
        assert!(!can_serve_vision_on_machine(
            &vl_catalog(8.0, 12.0, 4.7, 8.0),
            &[ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "GTX 1650 SUPER".into(),
                vram_gb: Some(4),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            }],
            32
        ));
        let card8 = build_virtual_card(&[ComputeDevice {
            id: "nvidia:0".into(),
            kind: "discrete".into(),
            name: "RTX 4060".into(),
            vram_gb: Some(8),
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        }])
        .unwrap();
        assert!(can_host_model(
            &vl_catalog(8.0, 12.0, 4.7, 8.0),
            &card8,
            32,
            2
        ));
        assert!(!can_serve_vision_on_card(
            &vl_catalog(8.0, 12.0, 4.7, 8.0),
            &card8
        ));
        let card12 = build_virtual_card(&[ComputeDevice {
            id: "nvidia:0".into(),
            kind: "discrete".into(),
            name: "RTX 3060".into(),
            vram_gb: Some(12),
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        }])
        .unwrap();
        assert!(can_host_model(
            &vl_catalog(8.0, 12.0, 4.7, 8.0),
            &card12,
            32,
            2
        ));
        assert!(can_serve_vision_on_card(
            &vl_catalog(8.0, 12.0, 4.7, 8.0),
            &card12
        ));
    }

    #[test]
    fn vision_vram_uses_catalog_then_hosting_floor() {
        assert_eq!(image_job_min_vram_gb(&vl_catalog(8.0, 12.0, 4.7, 8.0)), 12);
        let mut model = vl_catalog(8.0, 99.0, 4.7, 8.0);
        model.min_vram_gb_vision = None;
        assert_eq!(image_job_min_vram_gb(&model), 8);
    }
}
