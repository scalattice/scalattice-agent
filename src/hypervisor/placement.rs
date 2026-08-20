use crate::compute_pool::{ComputePlan, ComputeSlot, PoolStrategy};
use crate::models::{can_host_model, can_serve_vision_on_card, hosting_min_vram_gb, image_job_min_vram_gb};
use crate::protocol::CatalogModel;
use tracing::debug;

#[derive(Debug, Clone)]
pub struct Placement {
    /// Slot ids to claim (one for single-GPU/Vulkan/CPU; all TP siblings for tensor-parallel).
    pub slot_ids: Vec<String>,
    /// Card the worker should load with (may be TP card spanning siblings).
    pub card: crate::compute_pool::VirtualCard,
    /// Physical CUDA indices for the worker CVD (empty = hide CUDA).
    pub cuda_visible: Vec<u32>,
    pub use_tp_worker: bool,
}

/// Prefer the smallest idle accelerator that can **fully** host the model;
/// fall back to TP group, then GPU offload, then CPU.
pub fn pick_placement(
    plan: &ComputePlan,
    idle_slot_ids: &[String],
    model: &CatalogModel,
    ram_gb: u32,
    cpu_ram_headroom_gb: u32,
    devices: &[crate::specs::ComputeDevice],
    need_vision: bool,
) -> Option<Placement> {
    let idle: std::collections::HashSet<&str> =
        idle_slot_ids.iter().map(|s| s.as_str()).collect();

    let min_vram = if need_vision {
        image_job_min_vram_gb(model)
    } else {
        hosting_min_vram_gb(model)
    };

    let mut full_fit: Vec<&ComputeSlot> = plan
        .slots
        .iter()
        .filter(|s| idle.contains(s.id.as_str()) && s.kind != "cpu")
        .filter(|s| min_vram > 0 && s.card.total_vram_gb >= min_vram)
        .filter(|s| can_host_model(model, &s.card, ram_gb, cpu_ram_headroom_gb))
        .filter(|s| !need_vision || can_serve_vision_on_card(model, &s.card))
        .collect();
    full_fit.sort_by(|a, b| {
        a.card
            .total_vram_gb
            .cmp(&b.card.total_vram_gb)
            .then_with(|| a.priority.cmp(&b.priority))
            .then_with(|| a.id.cmp(&b.id))
    });
    if let Some(slot) = full_fit.first() {
        debug!(slot = %slot.id, "placement: single slot full fit");
        return Some(Placement {
            slot_ids: vec![slot.id.clone()],
            card: slot.card.clone(),
            cuda_visible: slot.cuda_visible.clone(),
            use_tp_worker: false,
        });
    }

    // Tensor-parallel before offload/CPU when pooled VRAM can fully host.
    for (group_id, phys_ids) in &plan.tp_groups {
        let siblings: Vec<&ComputeSlot> = plan
            .slots
            .iter()
            .filter(|s| s.tp_group.as_deref() == Some(group_id.as_str()))
            .collect();
        if siblings.is_empty() || siblings.iter().any(|s| !idle.contains(s.id.as_str())) {
            continue;
        }
        let Ok(tp_card) = crate::compute_pool::build_tp_card_for_group(devices, phys_ids) else {
            continue;
        };
        if tp_card.strategy != PoolStrategy::TensorParallel {
            continue;
        }
        if need_vision && !can_serve_vision_on_card(model, &tp_card) {
            continue;
        }
        if min_vram > 0 && tp_card.total_vram_gb < min_vram {
            continue;
        }
        if !can_host_model(model, &tp_card, ram_gb, cpu_ram_headroom_gb) {
            continue;
        }
        debug!(group = %group_id, "placement: tensor-parallel group");
        return Some(Placement {
            slot_ids: siblings.iter().map(|s| s.id.clone()).collect(),
            card: tp_card,
            cuda_visible: phys_ids.clone(),
            use_tp_worker: true,
        });
    }

    // Offload on the largest idle accelerator (text only — image jobs need full vision VRAM).
    if !need_vision {
    let mut offload: Vec<&ComputeSlot> = plan
        .slots
        .iter()
        .filter(|s| idle.contains(s.id.as_str()) && s.kind != "cpu")
        .filter(|s| can_host_model(model, &s.card, ram_gb, cpu_ram_headroom_gb))
        .collect();
    offload.sort_by(|a, b| {
        b.card
            .total_vram_gb
            .cmp(&a.card.total_vram_gb)
            .then_with(|| a.priority.cmp(&b.priority))
    });
    if let Some(slot) = offload.first() {
        debug!(slot = %slot.id, "placement: accelerator offload");
        return Some(Placement {
            slot_ids: vec![slot.id.clone()],
            card: slot.card.clone(),
            cuda_visible: slot.cuda_visible.clone(),
            use_tp_worker: false,
        });
    }

    if let Some(slot) = plan
        .slots
        .iter()
        .filter(|s| idle.contains(s.id.as_str()) && s.kind == "cpu")
        .find(|s| can_host_model(model, &s.card, ram_gb, cpu_ram_headroom_gb))
    {
        debug!(slot = %slot.id, "placement: cpu overflow");
        return Some(Placement {
            slot_ids: vec![slot.id.clone()],
            card: slot.card.clone(),
            cuda_visible: slot.cuda_visible.clone(),
            use_tp_worker: false,
        });
    }
    }

    None
}

/// Explain why [`pick_placement`] returned `None`. Vision misses with idle
/// slots are capacity (insufficient VRAM), not "busy".
pub fn placement_miss_detail(
    plan: &ComputePlan,
    idle_slot_ids: &[String],
    model: &CatalogModel,
    need_vision: bool,
) -> String {
    let model_id = model.model_id.as_str();
    let idle: std::collections::HashSet<&str> =
        idle_slot_ids.iter().map(|s| s.as_str()).collect();
    let idle_accel: Vec<&ComputeSlot> = plan
        .slots
        .iter()
        .filter(|s| idle.contains(s.id.as_str()) && s.kind != "cpu")
        .collect();

    if idle_accel.is_empty() && idle_slot_ids.is_empty() {
        return format!("agent_busy: no idle compute slot for {model_id}");
    }
    if idle_accel.is_empty() {
        // Only CPU idle — vision cannot use it; text would have placed CPU.
        if need_vision {
            let need = image_job_min_vram_gb(model);
            return format!(
                "insufficient_vram: need {need} GB GPU for vision job {model_id}; no idle accelerator"
            );
        }
        return format!("agent_busy: no idle compute slot for {model_id}");
    }

    if need_vision {
        let need = image_job_min_vram_gb(model);
        let max_slot = idle_accel
            .iter()
            .map(|s| s.card.total_vram_gb)
            .max()
            .unwrap_or(0);
        let mut max_pool = max_slot;
        for (group_id, _phys_ids) in &plan.tp_groups {
            let siblings: Vec<&ComputeSlot> = plan
                .slots
                .iter()
                .filter(|s| s.tp_group.as_deref() == Some(group_id.as_str()))
                .collect();
            if siblings.is_empty() || siblings.iter().any(|s| !idle.contains(s.id.as_str())) {
                continue;
            }
            let pooled: u32 = siblings.iter().map(|s| s.card.total_vram_gb).sum();
            max_pool = max_pool.max(pooled);
        }
        return format!(
            "insufficient_vram: need {need} GB GPU for vision job {model_id}; largest idle {max_pool} GB across {} slot(s)",
            idle_accel.len()
        );
    }

    format!("agent_busy: no placeable idle slot for {model_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute_pool::build_compute_slots;
    use crate::specs::ComputeDevice;

    fn model(min_vram: f64, weight: f64) -> CatalogModel {
        CatalogModel {
            model_id: "m".into(),
            display_name: "m".into(),
            runtime_model: "m".into(),
            max_context_tokens: 4096,
            regions: vec![],
            weight_size_gb: Some(weight),
            min_vram_gb: Some(min_vram),
            min_vram_gb_vision: None,
            vision_model: false,
            text_sibling_model_id: None,
            min_ram_gb: Some(4.0),
            mmproj_size_gb: None,
            vision_max_images: None,
            vision_max_image_side_px: None,
            vision_max_image_pixels: None,
            weights: None,
        }
    }

    #[test]
    fn prefers_smallest_fitting_cuda_slot() {
        let devices = [
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "Small".into(),
                vram_gb: Some(8),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "nvidia:1".into(),
                kind: "discrete".into(),
                name: "Large".into(),
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
                enabled: true,
            },
        ];
        let plan = build_compute_slots(&devices).unwrap();
        let idle: Vec<String> = plan.slots.iter().map(|s| s.id.clone()).collect();
        let placement = pick_placement(&plan, &idle, &model(8.0, 5.0), 64, 2, &devices, false).unwrap();
        assert_eq!(placement.slot_ids, vec!["cuda-0".to_string()]);
        assert!(!placement.use_tp_worker);
    }

    #[test]
    fn matched_large_model_uses_tp_group() {
        let devices = [
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "RTX 4090".into(),
                vram_gb: Some(24),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "nvidia:1".into(),
                kind: "discrete".into(),
                name: "RTX 4090".into(),
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
                enabled: true,
            },
        ];
        let plan = build_compute_slots(&devices).unwrap();
        let idle: Vec<String> = plan.slots.iter().map(|s| s.id.clone()).collect();
        let placement =
            pick_placement(&plan, &idle, &model(40.0, 30.0), 64, 2, &devices, false).unwrap();
        assert!(placement.use_tp_worker);
        assert_eq!(placement.slot_ids.len(), 2);
    }
}
