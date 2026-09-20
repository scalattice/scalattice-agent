use crate::compute_pool::{
    build_compute_slots, build_tp_card_for_group, build_virtual_card, PoolStrategy, VirtualCard,
};
use crate::protocol::CatalogModel;
use crate::specs::ComputeDevice;

use super::vram_plan::{
    extras_without_kv_gb, gpu_weights_need_gb, job_n_ctx, kv_gb,
};

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

fn llama_weight_gb(model: &CatalogModel) -> f64 {
    model
        .weight_size_gb
        .filter(|w| *w > 0.05)
        .or_else(|| {
            super::storage::resolve_model_gguf(&model.runtime_model).and_then(|path| {
                std::fs::metadata(path)
                    .ok()
                    .map(|m| m.len() as f64 / (1024.0 * 1024.0 * 1024.0))
            })
        })
        .unwrap_or(0.0)
}

fn llama_shape(model: &CatalogModel) -> Option<super::gguf_arch::GgufShape> {
    super::storage::resolve_model_gguf(&model.runtime_model)
        .and_then(|path| super::gguf_arch::gguf_shape(&path))
}

fn mmproj_gb(model: &CatalogModel, need_vision: bool) -> f64 {
    if need_vision {
        model.mmproj_size_gb.filter(|v| *v > 0.0).unwrap_or(0.0)
    } else {
        0.0
    }
}

fn llama_job_parts(model: &CatalogModel, need_vision: bool) -> (u32, f64, f64, f64, f64) {
    let n_ctx = job_n_ctx(model, need_vision);
    let weight = llama_weight_gb(model);
    let shape = llama_shape(model);
    let kv = kv_gb(weight, shape, n_ctx);
    let extras = extras_without_kv_gb(weight, shape, n_ctx);
    (n_ctx, weight, kv, extras, mmproj_gb(model, need_vision))
}

pub fn gpu_full_host_need_gb_for_job(model: &CatalogModel, need_vision: bool) -> f64 {
    if !need_vision {
        if let Some(v) = model.catalog_gpu_full_vram_gb() {
            return v.max(f64::from(hosting_min_vram_gb(model)));
        }
    }
    let (_n_ctx, weight, kv, extras, mmproj) = llama_job_parts(model, need_vision);
    let need = weight + kv + extras + mmproj;
    let catalog_floor = if need_vision {
        f64::from(image_job_min_vram_gb(model))
    } else {
        f64::from(hosting_min_vram_gb(model))
    };
    need.max(catalog_floor)
}

fn gpu_weights_need_gb_for_job(model: &CatalogModel, need_vision: bool) -> f64 {
    if !need_vision {
        if let Some(v) = model.catalog_gpu_weights_vram_gb() {
            return v;
        }
    }
    let n_ctx = job_n_ctx(model, need_vision);
    let weight = llama_weight_gb(model);
    gpu_weights_need_gb(weight, llama_shape(model), n_ctx) + mmproj_gb(model, need_vision)
}

fn job_kv_gb(model: &CatalogModel, need_vision: bool) -> f64 {
    if !need_vision {
        if let Some(v) = model.catalog_kv_cache_gb() {
            return v;
        }
    }
    llama_job_parts(model, need_vision).2
}

fn kv_ram_need_gb(model: &CatalogModel, need_vision: bool, cpu_ram_headroom_gb: u32) -> u32 {
    let kv = job_kv_gb(model, need_vision);
    let min_ram = gb_ceil(model.min_ram_gb);
    gb_ceil(Some(kv)).saturating_add(cpu_ram_headroom_gb).max(min_ram).max(1)
}

fn weight_plus_kv_ram_need_gb(model: &CatalogModel, need_vision: bool, cpu_ram_headroom_gb: u32) -> u32 {
    let weight = llama_weight_gb(model);
    let kv = job_kv_gb(model, need_vision);
    let min_ram = gb_ceil(model.min_ram_gb);
    gb_ceil(Some(weight + kv))
        .saturating_add(cpu_ram_headroom_gb)
        .max(min_ram)
}

fn unified_pool_gb(card: &VirtualCard, ram_gb: u32) -> f64 {
    f64::from(card.total_vram_gb.max(ram_gb))
}

/// True when leftover VRAM can hold the catalog KV (llama.cpp offload_kqv=true).
pub fn kv_fits_on_gpu(
    available_gb: f64,
    model: &CatalogModel,
    need_vision: bool,
) -> bool {
    available_gb + 0.005 >= gpu_full_host_need_gb_for_job(model, need_vision)
}

/// Catalog window + whether llama.cpp should keep KV on the GPU.
/// `offload_kqv=false` puts KV in system RAM when leftover VRAM cannot hold it.
pub fn llama_context_plan(
    model: &CatalogModel,
    card: &VirtualCard,
    need_vision: bool,
) -> (u32, bool) {
    let n_ctx = job_n_ctx(model, need_vision).max(1);
    let offload_kqv = match card.strategy {
        PoolStrategy::CpuOnly => false,
        PoolStrategy::Metal => true,
        _ => kv_fits_on_gpu(f64::from(card.total_vram_gb), model, need_vision),
    };
    (n_ctx, offload_kqv)
}

/// True when `available_gb` (live free, else advertised) can take weights + KV
/// without CPU offload. Float slop only: not a 50 MB "maybe it fits" gift.
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

/// GPU floor for VL image attachments: catalog `minVramGbVision`.
/// Diffusers picture SKUs use `hosting_min_vram_gb` (catalog `minVramGb`) via
/// `can_host_image_model` — they are not VL.
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

pub fn can_serve_vision_on_card(model: &CatalogModel, card: &VirtualCard) -> bool {
    let need = image_job_min_vram_gb(model);
    if need == 0 {
        return true;
    }
    card.total_vram_gb >= need && !matches!(card.strategy, PoolStrategy::CpuOnly)
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

fn can_host_image_model(model: &CatalogModel, card: &VirtualCard) -> bool {
    if matches!(
        card.strategy,
        PoolStrategy::TensorParallel | PoolStrategy::CpuOnly
    ) {
        return false;
    }
    if !crate::image::image_card_eligible(card) {
        return false;
    }
    match card.strategy {
        PoolStrategy::Metal if !crate::image::metal_image_available() => return false,
        _ => {}
    }
    let min_vram = hosting_min_vram_gb(model);
    min_vram == 0 || card.total_vram_gb >= min_vram
}

/// Whether this machine can download and serve a catalog model on its virtual compute card.
/// Chat/VL: catalog n_ctx KV must fit in leftover VRAM **or** system RAM (counted, not assumed).
/// Image-gen: one GPU ≥ catalog minVram, no token KV.
/// `cpu_ram_headroom_gb` comes from the server (`ready.cpuRamHeadroomGb`).
pub fn can_host_model(
    model: &CatalogModel,
    card: &VirtualCard,
    ram_gb: u32,
    cpu_ram_headroom_gb: u32,
) -> bool {
    if model.is_image_job() {
        return can_host_image_model(model, card);
    }
    let need_vision = false;
    let unified = matches!(card.strategy, PoolStrategy::Metal);
    if unified {
        let need = gpu_full_host_need_gb_for_job(model, need_vision);
        let min_ram = gb_ceil(model.min_ram_gb);
        return unified_pool_gb(card, ram_gb) + 0.005 >= need.max(f64::from(min_ram));
    }

    let vram = f64::from(card.total_vram_gb);
    if kv_fits_on_gpu(vram, model, need_vision) && ram_gb >= gb_ceil(model.min_ram_gb) {
        return true;
    }

    let has_accelerator = matches!(
        card.strategy,
        PoolStrategy::Single
            | PoolStrategy::TensorParallel
            | PoolStrategy::Vulkan
            | PoolStrategy::Metal
    );
    let weights_need = gpu_weights_need_gb_for_job(model, need_vision);
    if has_accelerator && vram + 0.005 >= weights_need {
        return ram_gb >= kv_ram_need_gb(model, need_vision, cpu_ram_headroom_gb);
    }

    // Layer offload: GPU keeps the majority of weights. 8B/9B on 4 GB + RAM
    // completes; 30B weights on 8 GB hangs the invoke timeout.
    if has_accelerator
        && layer_offload_fits(
            vram,
            weights_need,
            model,
            need_vision,
            ram_gb,
            cpu_ram_headroom_gb,
        )
    {
        return true;
    }
    if card.strategy == PoolStrategy::CpuOnly {
        return ram_gb >= weight_plus_kv_ram_need_gb(model, need_vision, cpu_ram_headroom_gb);
    }

    false
}

/// GPU must hold at least half the weights or CPU decode stalls.
const LAYER_OFFLOAD_MIN_GPU_FRACTION: f64 = 0.5;

fn layer_offload_fits(
    vram: f64,
    weights_need: f64,
    model: &CatalogModel,
    need_vision: bool,
    ram_gb: u32,
    cpu_ram_headroom_gb: u32,
) -> bool {
    if vram <= 0.0 || weights_need <= 0.0 {
        return false;
    }
    if vram + 0.005 < weights_need * LAYER_OFFLOAD_MIN_GPU_FRACTION {
        return false;
    }
    let spilled = (weights_need - vram).max(0.0);
    let ram_need = gb_ceil(Some(spilled + job_kv_gb(model, need_vision)))
        .saturating_add(cpu_ram_headroom_gb)
        .max(gb_ceil(model.min_ram_gb));
    ram_gb >= ram_need
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
    let has_accel = plan.slots.iter().any(|slot| slot.kind != "cpu");
    if plan.slots.iter().any(|slot| {
        if has_accel && slot.kind == "cpu" {
            return false;
        }
        can_host_model(model, &slot.card, ram_gb, cpu_ram_headroom_gb)
    }) {
        return true;
    }
    if model.is_image_job() {
        // Diffusers never claims a llama.cpp TP group as one GPU.
        return false;
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
            job_kind: String::new(),
            chat_template: String::new(),
            thinking: String::new(),
            usd_per_image: 0.0,
            image_max_n: 0,
            max_context_tokens: 4096,
            regions: vec![],
            weight_size_gb: Some(weight),
            min_vram_gb: Some(min_vram),
            min_vram_gb_vision: None,
            vision_model: false,
            text_sibling_model_id: None,
            min_ram_gb: Some(min_ram),
            kv_cache_gb: None,
            gpu_full_vram_gb: None,
            gpu_weights_vram_gb: None,
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
            job_kind: String::new(),
            chat_template: String::new(),
            thinking: String::new(),
            usd_per_image: 0.0,
            image_max_n: 0,
            max_context_tokens: 8192,
            regions: vec![],
            weight_size_gb: Some(weight),
            min_vram_gb: Some(min_vram),
            min_vram_gb_vision: Some(vision_vram),
            vision_model: true,
            text_sibling_model_id: Some("qwen-3-8b".into()),
            min_ram_gb: Some(min_ram),
            kv_cache_gb: None,
            gpu_full_vram_gb: None,
            gpu_weights_vram_gb: None,
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
        let need = gpu_full_host_need_gb_for_job(&qwen, false);
        assert!(need > 6.0, "{need}");
        assert!(need < 8.0, "{need}");
        assert!(!vram_can_gpu_full(6.0, &qwen, 4, false));
        assert!(vram_can_gpu_full(10.0, &qwen, 4, false));
        let eight_gb_ok = catalog(8.0, 5.0, 8.0);
        assert!(vram_can_gpu_full(8.0, &eight_gb_ok, 8, false));
    }

    #[test]
    fn catalog_fit_numbers_override_local_plan() {
        let mut m = catalog(4.0, 4.68, 8.0);
        m.max_context_tokens = 32768;
        m.gpu_full_vram_gb = Some(10.0);
        m.gpu_weights_vram_gb = Some(5.5);
        m.kv_cache_gb = Some(4.5);
        assert!((gpu_full_host_need_gb_for_job(&m, false) - 10.0).abs() < 0.01);
        assert!(!vram_can_gpu_full(8.0, &m, 4, false));
        let card = build_virtual_card(&[ComputeDevice {
            id: "nvidia:0".into(),
            kind: "discrete".into(),
            name: "RTX 4060".into(),
            vram_gb: Some(8),
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        }])
        .unwrap();
        assert!(can_host_model(&m, &card, 16, 2));
        assert!(!can_host_model(&m, &card, 6, 2));
        let (n_ctx, offload_kqv) = llama_context_plan(&m, &card, false);
        assert_eq!(n_ctx, 32768);
        assert!(!offload_kqv);
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
    fn tiny_vulkan_slot_does_not_host_coder_sized_weights() {
        let card = VirtualCard {
            devices: vec![],
            strategy: PoolStrategy::Vulkan,
            display_name: "1 GB Vulkan".into(),
            total_vram_gb: 1,
            tensor_split: vec![],
            cuda_device_ids: vec![],
            uses_vulkan: true,
            gpu_layer_budget: 0,
        };
        assert!(!can_host_model(&catalog(8.0, 17.0, 16.0), &card, 32, 2));
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
    fn dual_2gb_tp_does_not_host_coder_via_ram_offload() {
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
        assert!(!can_host_model(&catalog(12.0, 11.7, 16.0), &card, 16, 2));
        assert!(!can_host_on_machine(
            &catalog(12.0, 11.7, 16.0),
            &devices,
            16,
            2
        ));
    }

    #[test]
    fn eight_gb_gpu_does_not_host_coder_sized_weights() {
        let devices = [
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "NVIDIA GeForce RTX 5050".into(),
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
        ];
        let gpu = [ComputeDevice {
            id: "nvidia:0".into(),
            kind: "discrete".into(),
            name: "NVIDIA GeForce RTX 5050".into(),
            vram_gb: Some(8),
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        }];
        let card = build_virtual_card(&gpu).unwrap();
        let coder = catalog(22.5, 19.0, 24.0);
        assert!(!can_host_model(&coder, &card, 31, 2));
        assert!(!can_host_on_machine(&coder, &devices, 31, 2));
    }

    #[test]
    fn four_gb_hosts_eight_b_when_ram_covers_spilled_layers() {
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
        let eight_b = catalog(10.7, 5.0, 8.0);
        assert!(can_host_model(&eight_b, &card, 16, 2));
        assert!(!can_host_model(&eight_b, &card, 6, 2));
        let fourteen_b = catalog(13.2, 9.0, 12.0);
        assert!(!can_host_model(&fourteen_b, &card, 16, 2));
    }

    #[test]
    fn eight_gb_hosts_fourteen_b_offload_but_not_coder() {
        let card = build_virtual_card(&[ComputeDevice {
            id: "nvidia:0".into(),
            kind: "discrete".into(),
            name: "RTX 5050".into(),
            vram_gb: Some(8),
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        }])
        .unwrap();
        assert!(can_host_model(&catalog(13.2, 9.0, 12.0), &card, 31, 2));
        assert!(!can_host_model(&catalog(22.5, 19.0, 24.0), &card, 31, 2));
    }

    #[test]
    fn vl_text_can_offload_on_four_gb_images_need_vision_vram() {
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

    fn image_catalog(min_vram: f64) -> CatalogModel {
        CatalogModel {
            model_id: "qwen-image".into(),
            display_name: "Qwen Image".into(),
            runtime_model: "Qwen/Qwen-Image".into(),
            job_kind: "image".into(),
            chat_template: String::new(),
            thinking: String::new(),
            usd_per_image: 0.03,
            image_max_n: 1,
            max_context_tokens: 0,
            regions: vec![],
            weight_size_gb: Some(40.0),
            min_vram_gb: Some(min_vram),
            min_vram_gb_vision: None,
            vision_model: false,
            text_sibling_model_id: None,
            min_ram_gb: Some(16.0),
            kv_cache_gb: None,
            gpu_full_vram_gb: None,
            gpu_weights_vram_gb: None,
            mmproj_size_gb: None,
            vision_max_images: None,
            vision_max_image_side_px: None,
            vision_max_image_pixels: None,
            weights: None,
        }
    }

    #[test]
    fn amd_discrete_hosts_image_sku() {
        if !vulkan_runtime_supported() {
            return;
        }
        let card = build_virtual_card(&[ComputeDevice {
            id: "amd:0".into(),
            kind: "discrete".into(),
            name: "AMD Radeon RX 7900 XTX".into(),
            vram_gb: Some(24),
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        }])
        .unwrap();
        assert!(can_host_model(&image_catalog(16.0), &card, 32, 2));
        assert!(!can_host_model(&image_catalog(48.0), &card, 32, 2));
    }

    #[test]
    fn cpu_does_not_host_image_sku() {
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
        assert!(!can_host_model(&image_catalog(8.0), &card, 64, 2));
    }

    fn dual_3090s() -> [ComputeDevice; 3] {
        [
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "NVIDIA GeForce RTX 3090".into(),
                vram_gb: Some(24),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "nvidia:1".into(),
                kind: "discrete".into(),
                name: "NVIDIA GeForce RTX 3090".into(),
                vram_gb: Some(24),
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
                enabled: false,
            },
        ]
    }

    #[test]
    fn dual_24gb_hosts_image_at_catalog_floor_not_gguf_inflate() {
        let devices = dual_3090s();
        // Catalog 24 GB floor: one 3090 is enough. GGUF-style 45.3 (weight+KV)
        // must not be treated as a single-GPU picture requirement via TP pooling.
        assert!(can_host_on_machine(
            &image_catalog(24.0),
            &devices,
            63,
            2
        ));
        assert!(!can_host_on_machine(
            &image_catalog(45.3),
            &devices,
            63,
            2
        ));
    }

    #[test]
    fn intel_arc_hosts_image_sku() {
        if !vulkan_runtime_supported() {
            return;
        }
        let card = build_virtual_card(&[ComputeDevice {
            id: "pci-intel:0".into(),
            kind: "discrete".into(),
            name: "Intel Arc B580".into(),
            vram_gb: Some(12),
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        }])
        .unwrap();
        assert!(can_host_model(&image_catalog(8.0), &card, 32, 2));
        assert!(!can_host_model(&image_catalog(24.0), &card, 32, 2));
    }
}
