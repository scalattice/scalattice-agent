use crate::compute_pool::{ComputePlan, ComputeSlot, PoolStrategy};
use crate::models::{
    can_host_model_at, can_serve_vision_on_card, cpu_slot_may_serve, hosting_min_vram_gb,
    image_job_min_vram_gb, occupancy_min_vram_gb, placement_sys_ram_need_gb, vram_can_gpu_full_at,
};
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

fn image_slot_backend_rank(slot: &ComputeSlot) -> u8 {
    if matches!(slot.card.strategy, PoolStrategy::Single) && !slot.card.uses_vulkan {
        0
    } else if matches!(slot.card.strategy, PoolStrategy::Metal) {
        1
    } else if crate::image::is_amd_discrete_card(&slot.card) {
        2
    } else {
        3
    }
}

fn pick_image_placement(
    plan: &ComputePlan,
    idle: &std::collections::HashSet<&str>,
    model: &CatalogModel,
) -> Option<Placement> {
    let min_vram = hosting_min_vram_gb(model);
    let mut candidates: Vec<&ComputeSlot> = plan
        .slots
        .iter()
        .filter(|s| idle.contains(s.id.as_str()))
        .filter(|s| s.kind != "cpu" && crate::image::image_card_eligible(&s.card))
        .filter(|s| min_vram == 0 || s.card.total_vram_gb >= min_vram)
        .collect();
    candidates.sort_by(|a, b| {
        image_slot_backend_rank(a)
            .cmp(&image_slot_backend_rank(b))
            .then_with(|| a.card.total_vram_gb.cmp(&b.card.total_vram_gb))
            .then_with(|| a.priority.cmp(&b.priority))
            .then_with(|| a.id.cmp(&b.id))
    });
    let slot = candidates.first()?;
    let cuda_visible = if slot.cuda_visible.is_empty() {
        if crate::image::is_amd_discrete_card(&slot.card) {
            crate::image::amd_visible_index(&slot.card)
        } else if crate::image::is_intel_arc_card(&slot.card) {
            crate::image::intel_visible_index(&slot.card)
        } else {
            slot.cuda_visible.clone()
        }
    } else {
        slot.cuda_visible.clone()
    };
    Some(Placement {
        slot_ids: vec![slot.id.clone()],
        card: slot.card.clone(),
        cuda_visible,
        use_tp_worker: false,
    })
}

/// Build a placement that runs on the server-chosen slot.
/// No GPU↔CPU remapping. Dual-mode TP: expand a preferred pin to the full
/// TpGroup only when the single pin card cannot fully host the model but the
/// pooled TP card can. Small jobs that fit one sibling stay on that pin so
/// other siblings remain free. Caller must verify all returned slot_ids are idle.
pub fn placement_for_required_slot(
    plan: &ComputePlan,
    slot_id: &str,
    devices: &[crate::specs::ComputeDevice],
    model: &CatalogModel,
    need_vision: bool,
    n_ctx: u32,
) -> Option<Placement> {
    let want = slot_id.trim();
    if want.is_empty() {
        return None;
    }
    let slot = plan.slots.iter().find(|s| s.id == want)?;
    // Image / Diffusers jobs never use the TP worker path.
    if !model.is_image_job() {
        if let Some(group_id) = slot.tp_group.as_deref().filter(|g| !g.is_empty()) {
            let siblings: Vec<&ComputeSlot> = plan
                .slots
                .iter()
                .filter(|s| s.tp_group.as_deref() == Some(group_id))
                .collect();
            if siblings.len() > 1 {
                let min_vram = if need_vision {
                    image_job_min_vram_gb(model)
                } else {
                    hosting_min_vram_gb(model)
                };
                let pin_can_full = vram_can_gpu_full_at(
                    f64::from(slot.card.total_vram_gb),
                    model,
                    min_vram,
                    need_vision,
                    n_ctx,
                ) && (!need_vision
                    || can_serve_vision_on_card(model, &slot.card));
                if !pin_can_full {
                    let phys_ids: Vec<u32> = siblings
                        .iter()
                        .flat_map(|s| s.cuda_visible.iter().copied())
                        .collect();
                    if let Ok(tp_card) =
                        crate::compute_pool::build_tp_card_for_group(devices, &phys_ids)
                    {
                        if tp_card.strategy == PoolStrategy::TensorParallel {
                            let tp_can_full = vram_can_gpu_full_at(
                                f64::from(tp_card.total_vram_gb),
                                model,
                                min_vram,
                                need_vision,
                                n_ctx,
                            ) && (!need_vision
                                || can_serve_vision_on_card(model, &tp_card));
                            if tp_can_full {
                                debug!(
                                    pin = %slot.id,
                                    group = %group_id,
                                    "placement: preferred pin expands to TP (pool required)"
                                );
                                return Some(Placement {
                                    slot_ids: siblings.iter().map(|s| s.id.clone()).collect(),
                                    card: tp_card,
                                    cuda_visible: phys_ids,
                                    use_tp_worker: true,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    Some(Placement {
        slot_ids: vec![slot.id.clone()],
        card: slot.card.clone(),
        cuda_visible: slot.cuda_visible.clone(),
        use_tp_worker: false,
    })
}

/// Prefer the smallest idle accelerator that can **fully** host the model
/// (weights + KV headroom). If none are free, place on the largest idle
/// accelerator that can still hold the weights (KV may live in RAM), or
/// layer-offload when the GPU still holds most of the weights
/// (see LAYER_OFFLOAD_MIN_GPU_FRACTION).
/// When no accelerator can host, place on the CPU slot if system RAM covers
/// weights + KV + headroom. Image jobs never use the CPU. A busy accelerator
/// that could host the model is not replaced by the CPU.
///
/// Legacy only: production invokes should pin `preferredSlotId` from the router
/// and use [`placement_for_required_slot`] instead.
#[cfg(test)]
pub fn pick_placement(
    plan: &ComputePlan,
    idle_slot_ids: &[String],
    model: &CatalogModel,
    ram_gb: u32,
    cpu_ram_headroom_gb: u32,
    devices: &[crate::specs::ComputeDevice],
    need_vision: bool,
) -> Option<Placement> {
    pick_placement_with_cpu(
        plan,
        idle_slot_ids,
        model,
        ram_gb,
        cpu_ram_headroom_gb,
        devices,
        need_vision,
        crate::specs::cpu_logical_cores(),
        ram_gb.saturating_sub(crate::specs::detect_ram_used_gb().unwrap_or(0)),
        &std::collections::HashSet::new(),
        &std::collections::HashMap::new(),
        false,
        0,
    )
}

pub fn pick_placement_with_cpu(
    plan: &ComputePlan,
    idle_slot_ids: &[String],
    model: &CatalogModel,
    ram_gb: u32,
    cpu_ram_headroom_gb: u32,
    devices: &[crate::specs::ComputeDevice],
    need_vision: bool,
    cpu_logical_cores: u32,
    sys_ram_available_gb: u32,
    // Idle slots holding our warm weights. Live free looks low, but
    // model_cache will evict them before the cold load.
    reclaimable_slot_ids: &std::collections::HashSet<String>,
    // System RAM freed when that slot's warm resident is evicted (GB).
    reclaim_sys_ram_gb: &std::collections::HashMap<String, u32>,
    // When true, preferredSlotId pinned a CPU slot — honor it even for GPU-class models.
    force_cpu_pin: bool,
    // Job llama n_ctx (0 = catalog maxContextTokens).
    n_ctx: u32,
) -> Option<Placement> {
    let idle: std::collections::HashSet<&str> = idle_slot_ids.iter().map(|s| s.as_str()).collect();

    if model.is_image_job() {
        return pick_image_placement(plan, &idle, model);
    }

    let min_vram = if need_vision {
        image_job_min_vram_gb(model)
    } else {
        hosting_min_vram_gb(model)
    };

    let live_cuda = crate::specs::live_cuda_free_vram_by_index();
    // Soft-estimate iGPUs (AMD "Radeon Graphics" at 2 GB) must not steal placement
    // when a discrete/Metal card nameplate-hosts the model (fleet: DESKTOP-SJVL4OL).
    let discrete_nameplate_hosts = plan.slots.iter().any(|s| {
        s.kind != "cpu"
            && s.kind != "integrated"
            && can_host_model_at(model, &s.card, ram_gb, cpu_ram_headroom_gb, n_ctx)
            && (!need_vision || can_serve_vision_on_card(model, &s.card))
    });

    let mut full_fit: Vec<&ComputeSlot> = plan
        .slots
        .iter()
        .filter(|s| idle.contains(s.id.as_str()) && s.kind != "cpu")
        .filter(|s| !(discrete_nameplate_hosts && s.kind == "integrated"))
        .filter(|s| {
            vram_can_gpu_full_at(
                slot_available_gb(s, &live_cuda, reclaimable_slot_ids.contains(&s.id)),
                model,
                min_vram,
                need_vision,
                n_ctx,
            )
        })
        .filter(|s| can_host_model_at(model, &s.card, ram_gb, cpu_ram_headroom_gb, n_ctx))
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
        if !vram_can_gpu_full_at(
            tp_available_gb(&tp_card, &live_cuda),
            model,
            min_vram,
            need_vision,
            n_ctx,
        ) {
            continue;
        }
        if !can_host_model_at(model, &tp_card, ram_gb, cpu_ram_headroom_gb, n_ctx) {
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

    // Offload on the largest idle accelerator (text only: image jobs need full vision VRAM).
    if !need_vision {
        let mut offload: Vec<&ComputeSlot> = plan
            .slots
            .iter()
            .filter(|s| idle.contains(s.id.as_str()) && s.kind != "cpu")
            .filter(|s| !(discrete_nameplate_hosts && s.kind == "integrated"))
            .filter(|s| {
                accelerator_live_can_place(
                    s,
                    &live_cuda,
                    model,
                    reclaimable_slot_ids.contains(&s.id),
                )
            })
            .filter(|s| can_host_model_at(model, &s.card, ram_gb, cpu_ram_headroom_gb, n_ctx))
            .filter(|s| {
                let credit = reclaim_sys_ram_gb.get(&s.id).copied().unwrap_or(0);
                placement_sys_ram_need_gb(model, &s.card, need_vision, cpu_ram_headroom_gb)
                    <= sys_ram_available_gb.saturating_add(credit)
            })
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
            .find(|s| can_host_model_at(model, &s.card, ram_gb, cpu_ram_headroom_gb, n_ctx))
        {
            // GPU-catalog models on a box that has any accelerator must not silently
            // crawl on cpu-0 (fleet: DESKTOP-SJVL4OL 27B/35B on cpu → invoke_timeout).
            // preferredSlotId=cpu-* sets force_cpu_pin so admin/debug can still target CPU.
            let machine_has_accel = plan.slots.iter().any(|s| s.kind != "cpu");
            let weight_gb = model.weight_size_gb.unwrap_or(0.0);
            let model_expects_gpu = hosting_min_vram_gb(model) > 0 || weight_gb > 2.0;
            if machine_has_accel && model_expects_gpu && !force_cpu_pin {
                debug!(
                    slot = %slot.id,
                    "placement: skip cpu; GPU-class model on a machine with accelerators"
                );
                return None;
            }
            // Idle accelerator that nameplate-hosts this model but lacks live free
            // VRAM (foreign occupancy — not our reclaimable warm) → bounce for failover.
            let idle_accel_nameplate_hosts = plan.slots.iter().any(|s| {
                s.kind != "cpu"
                    && idle.contains(s.id.as_str())
                    && can_host_model_at(model, &s.card, ram_gb, cpu_ram_headroom_gb, n_ctx)
            });
            // A busy card that could host keeps the job off the CPU.
            let busy_accel_could_host = plan.slots.iter().any(|s| {
                s.kind != "cpu"
                    && !idle.contains(s.id.as_str())
                    && can_host_model_at(model, &s.card, ram_gb, cpu_ram_headroom_gb, n_ctx)
            });
            let cpu_ram_need =
                placement_sys_ram_need_gb(model, &slot.card, need_vision, cpu_ram_headroom_gb);
            let cpu_sys_avail = sys_ram_available_gb
                .saturating_add(reclaim_sys_ram_gb.get(&slot.id).copied().unwrap_or(0));
            if !force_cpu_pin && idle_accel_nameplate_hosts {
                debug!(
                    slot = %slot.id,
                    "placement: skip cpu; idle accelerator nameplate-hosts but live free is too small"
                );
            } else if !force_cpu_pin && busy_accel_could_host {
                debug!(slot = %slot.id, "placement: skip cpu; an accelerator can host this model");
            } else if !cpu_slot_may_serve(model, ram_gb, cpu_ram_headroom_gb, cpu_logical_cores) {
                debug!(
                    slot = %slot.id,
                    cores = cpu_logical_cores,
                    "placement: skip cpu; RAM or CPU capability gate"
                );
            } else if cpu_ram_need > cpu_sys_avail {
                debug!(
                    slot = %slot.id,
                    need = cpu_ram_need,
                    available = cpu_sys_avail,
                    "placement: skip cpu; system RAM already reserved by sibling slots"
                );
            } else {
                debug!(slot = %slot.id, force_cpu_pin, "placement: cpu slot");
                return Some(Placement {
                    slot_ids: vec![slot.id.clone()],
                    card: slot.card.clone(),
                    cuda_visible: slot.cuda_visible.clone(),
                    use_tp_worker: false,
                });
            }
        }
    }

    None
}

/// The server already decided this card's reported size can hold the model.
/// A slot that is busy with a job is excluded before this runs.
pub(crate) fn accelerator_live_can_place(
    slot: &ComputeSlot,
    live_cuda: &std::collections::HashMap<u32, f64>,
    model: &CatalogModel,
    reclaim_our_warm: bool,
) -> bool {
    if slot.kind == "cpu" {
        return false;
    }
    let available = slot_available_gb(slot, live_cuda, reclaim_our_warm);
    available + 0.005 >= occupancy_min_vram_gb(model)
}

/// Idle slot that already holds this model. A cold load checks live free VRAM;
/// reusing the resident copy does not.
pub fn pick_resident_placement(
    plan: &ComputePlan,
    idle_slot_ids: &[String],
    resident_slot_ids: &[String],
    model: &CatalogModel,
    ram_gb: u32,
    cpu_ram_headroom_gb: u32,
    need_vision: bool,
    n_ctx: u32,
) -> Option<Placement> {
    if resident_slot_ids.is_empty() || idle_slot_ids.is_empty() {
        return None;
    }
    let idle: std::collections::HashSet<&str> = idle_slot_ids.iter().map(|s| s.as_str()).collect();
    let mut hits: Vec<&ComputeSlot> = plan
        .slots
        .iter()
        .filter(|slot| {
            resident_slot_ids.iter().any(|id| id == &slot.id) && idle.contains(slot.id.as_str())
        })
        .filter(|slot| {
            resident_slot_can_serve(slot, model, ram_gb, cpu_ram_headroom_gb, need_vision, n_ctx)
        })
        .collect();
    if hits.is_empty() {
        return None;
    }
    hits.sort_by(|a, b| {
        let ak = u8::from(a.kind == "cpu");
        let bk = u8::from(b.kind == "cpu");
        ak.cmp(&bk)
            .then(a.priority.cmp(&b.priority))
            .then(a.id.cmp(&b.id))
    });
    let slot = hits[0];
    Some(Placement {
        slot_ids: vec![slot.id.clone()],
        card: slot.card.clone(),
        cuda_visible: slot.cuda_visible.clone(),
        use_tp_worker: matches!(slot.card.strategy, PoolStrategy::TensorParallel),
    })
}

fn resident_slot_can_serve(
    slot: &ComputeSlot,
    model: &CatalogModel,
    ram_gb: u32,
    cpu_ram_headroom_gb: u32,
    need_vision: bool,
    n_ctx: u32,
) -> bool {
    if slot.kind == "cpu" {
        return cpu_slot_may_serve(
            model,
            ram_gb,
            cpu_ram_headroom_gb,
            crate::specs::cpu_logical_cores(),
        );
    }
    if need_vision && !can_serve_vision_on_card(model, &slot.card) {
        return false;
    }
    can_host_model_at(model, &slot.card, ram_gb, cpu_ram_headroom_gb, n_ctx)
}

fn slot_available_gb(
    slot: &ComputeSlot,
    live_cuda: &std::collections::HashMap<u32, f64>,
    reclaim_our_warm: bool,
) -> f64 {
    // Our idle warm resident will be evicted in make_gpu_room before load —
    // do not treat that occupancy like a foreign process we must bounce around.
    if reclaim_our_warm {
        return f64::from(slot.card.total_vram_gb);
    }
    // Prefer live free VRAM so we refuse placements that nameplate-fit but cannot
    // actually offload (fleet: preload loops / insufficient_vram after claim).
    if let Some(free) = crate::gpu_occupancy::slot_live_free_gb(slot, live_cuda) {
        return free.max(0.0);
    }
    f64::from(slot.card.total_vram_gb)
}

fn tp_available_gb(
    card: &crate::compute_pool::VirtualCard,
    live_cuda: &std::collections::HashMap<u32, f64>,
) -> f64 {
    if matches!(card.strategy, PoolStrategy::TensorParallel) && !card.devices.is_empty() {
        // TP pools: use the tightest live free among member CUDA indices when known.
        let mut min_free: Option<f64> = None;
        for device in &card.devices {
            if let Some(idx) = device
                .id
                .strip_prefix("nvidia:")
                .and_then(|s| s.parse::<u32>().ok())
            {
                if let Some(free) = live_cuda.get(&idx).copied() {
                    min_free = Some(match min_free {
                        Some(existing) => existing.min(free),
                        None => free,
                    });
                }
            }
        }
        if let Some(free) = min_free {
            return free.max(0.0);
        }
    }
    f64::from(card.total_vram_gb)
}

/// Explain why [`pick_placement`] returned `None`. Vision misses with idle
/// slots are capacity (insufficient VRAM), not "busy": unless a fitting GPU
/// exists on the machine and is just occupied.
pub fn placement_miss_detail(
    plan: &ComputePlan,
    idle_slot_ids: &[String],
    model: &CatalogModel,
    need_vision: bool,
    reclaimable_slot_ids: &std::collections::HashSet<String>,
    sys_ram_available_gb: u32,
    reclaim_sys_ram_gb: &std::collections::HashMap<String, u32>,
    ram_gb: u32,
    cpu_ram_headroom_gb: u32,
    n_ctx: u32,
) -> crate::invoke_code::CodedError {
    let model_id = model.model_id.as_str();
    let idle: std::collections::HashSet<&str> = idle_slot_ids.iter().map(|s| s.as_str()).collect();
    let idle_accel: Vec<&ComputeSlot> = plan
        .slots
        .iter()
        .filter(|s| idle.contains(s.id.as_str()) && s.kind != "cpu")
        .collect();

    if idle_accel.is_empty() && idle_slot_ids.is_empty() {
        return crate::invoke_code::CodedError::new(
            crate::invoke_code::InvokeErrorCode::NoIdleSlot,
            format!("no idle compute slot for {model_id}"),
        );
    }

    if model.is_image_job() {
        let min_vram = hosting_min_vram_gb(model);
        let image_slots: Vec<&ComputeSlot> = plan
            .slots
            .iter()
            .filter(|s| {
                s.kind != "cpu"
                    && crate::image::image_card_eligible(&s.card)
                    && (min_vram == 0 || s.card.total_vram_gb >= min_vram)
            })
            .collect();
        if image_slots.iter().any(|s| idle.contains(s.id.as_str())) {
            return crate::invoke_code::CodedError::new(
                crate::invoke_code::InvokeErrorCode::NoIdleSlot,
                format!("no placeable idle slot for {model_id}"),
            );
        }
        if !image_slots.is_empty() {
            return crate::invoke_code::CodedError::new(
                crate::invoke_code::InvokeErrorCode::AgentBusy,
                format!("waiting for a GPU that can host {model_id} (need {min_vram} GB)"),
            );
        }
        let max_idle = idle_accel
            .iter()
            .map(|s| s.card.total_vram_gb)
            .max()
            .unwrap_or(0);
        return crate::invoke_code::CodedError::new(
            crate::invoke_code::InvokeErrorCode::InsufficientVram,
            format!("need {min_vram} GB GPU for image job {model_id}; largest idle {max_idle} GB"),
        );
    }

    let live_cuda = crate::specs::live_cuda_free_vram_by_index();
    // Discrete/Metal nameplate host (ignore soft-estimate iGPUs). Used so a busy
    // or mis-measured CUDA card hops as agent_busy instead of blaming a 2 GB
    // iGPU (fleet: DESKTOP-SJVL4OL). Evaluated after sys-RAM capacity misses so
    // tight offload RAM stays insufficient_vram, not busy.
    let discrete_nameplate_hosts = || {
        plan.slots.iter().any(|s| {
            s.kind != "cpu"
                && s.kind != "integrated"
                && can_host_model_at(model, &s.card, ram_gb, cpu_ram_headroom_gb, n_ctx)
                && (!need_vision || can_serve_vision_on_card(model, &s.card))
        })
    };
    let agent_busy_waiting_gpu = || {
        let need = occupancy_min_vram_gb(model);
        crate::invoke_code::CodedError::new(
            crate::invoke_code::InvokeErrorCode::AgentBusy,
            format!("waiting for a GPU that can fully host {model_id} (need {need:.1} GB)"),
        )
    };

    if idle_accel.is_empty() {
        // Only CPU idle: vision cannot use it; text would have placed CPU unless
        // sys-RAM / capability gates refused — that is capacity, not "no slot".
        // Prefer hop when a discrete card on this box nameplate-hosts the SKU.
        if !need_vision && discrete_nameplate_hosts() {
            return agent_busy_waiting_gpu();
        }
        if need_vision {
            let need = crate::models::gpu_full_host_need_gb_at(model, true, n_ctx)
                .ceil()
                .max(f64::from(image_job_min_vram_gb(model))) as u32;
            return crate::invoke_code::CodedError::new(
                crate::invoke_code::InvokeErrorCode::InsufficientVram,
                format!("need {need} GB GPU for vision job {model_id}; no idle accelerator"),
            );
        }
        if !idle_slot_ids.is_empty() {
            let cpu_slot = plan
                .slots
                .iter()
                .find(|s| idle.contains(s.id.as_str()) && s.kind == "cpu");
            if let Some(slot) = cpu_slot {
                let cpu_need =
                    placement_sys_ram_need_gb(model, &slot.card, false, cpu_ram_headroom_gb);
                let credit = reclaim_sys_ram_gb.get(&slot.id).copied().unwrap_or(0);
                let avail = sys_ram_available_gb.saturating_add(credit);
                if cpu_need > avail {
                    return crate::invoke_code::CodedError::new(
                        crate::invoke_code::InvokeErrorCode::InsufficientVram,
                        format!(
                            "need {cpu_need} GB free system RAM to run {model_id} on CPU; have {avail} GB"
                        ),
                    );
                }
            }
            return crate::invoke_code::CodedError::new(
                crate::invoke_code::InvokeErrorCode::InsufficientVram,
                format!("no accelerator can host {model_id}; CPU path refused (RAM or capability)"),
            );
        }
        return crate::invoke_code::CodedError::new(
            crate::invoke_code::InvokeErrorCode::NoIdleSlot,
            format!("no idle compute slot for {model_id}"),
        );
    }

    if need_vision {
        let need = crate::models::gpu_full_host_need_gb_at(model, true, n_ctx)
            .ceil()
            .max(f64::from(image_job_min_vram_gb(model))) as u32;
        // Vision needs one accelerator that fits — never sum sibling nameplates
        // (fleet: 2×3090 reported "largest idle 48 GB" while each slot is 24 GB).
        let max_slot = idle_accel
            .iter()
            .map(|s| s.card.total_vram_gb)
            .max()
            .unwrap_or(0);
        return crate::invoke_code::CodedError::new(
            crate::invoke_code::InvokeErrorCode::InsufficientVram,
            format!(
                "need {need} GB GPU for vision job {model_id}; largest idle slot {max_slot} GB ({} idle accelerator slot(s))",
                idle_accel.len()
            ),
        );
    }

    let max_free = idle_accel
        .iter()
        .map(|slot| slot_available_gb(slot, &live_cuda, reclaimable_slot_ids.contains(&slot.id)))
        .fold(0.0_f64, f64::max);
    let need = occupancy_min_vram_gb(model);
    // VRAM looks fine but every idle accelerator failed the sys-RAM offload gate
    // (fleet: Laptop / scalattice "need 3.2 · largest idle 10" while warm mmap held RAM).
    if max_free + 0.005 >= need {
        let mut min_sys_need = u32::MAX;
        let mut best_sys_avail = 0u32;
        for slot in &idle_accel {
            if !accelerator_live_can_place(
                slot,
                &live_cuda,
                model,
                reclaimable_slot_ids.contains(&slot.id),
            ) {
                continue;
            }
            let sys_need =
                placement_sys_ram_need_gb(model, &slot.card, need_vision, cpu_ram_headroom_gb);
            let credit = reclaim_sys_ram_gb.get(&slot.id).copied().unwrap_or(0);
            let sys_avail = sys_ram_available_gb.saturating_add(credit);
            if sys_need > sys_avail {
                min_sys_need = min_sys_need.min(sys_need);
                best_sys_avail = best_sys_avail.max(sys_avail);
            }
        }
        if min_sys_need != u32::MAX {
            // Capacity, not transient busy — debug UI must fail closed (not spin on
            // agent_busy), and the router should not keep retrying the same box.
            return crate::invoke_code::CodedError::new(
                crate::invoke_code::InvokeErrorCode::InsufficientVram,
                format!(
                    "need {min_sys_need} GB free system RAM to offload {model_id}; have {best_sys_avail} GB"
                ),
            );
        }
    }
    if discrete_nameplate_hosts() {
        return agent_busy_waiting_gpu();
    }
    crate::invoke_code::CodedError::new(
        crate::invoke_code::InvokeErrorCode::InsufficientVram,
        format!(
            "need {need:.1} GB on a graphics card for {model_id}; largest idle card is {max_free:.1} GB"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute_pool::build_compute_slots;
    use crate::specs::ComputeDevice;
    use std::collections::{HashMap, HashSet};

    fn model(min_vram: f64, weight: f64) -> CatalogModel {
        CatalogModel {
            model_id: "m".into(),
            display_name: "m".into(),
            runtime_model: "m".into(),
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
            min_ram_gb: Some(4.0),
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
        let placement =
            pick_placement(&plan, &idle, &model(8.0, 5.0), 64, 2, &devices, false).unwrap();
        assert_eq!(placement.slot_ids, vec!["cuda-0".to_string()]);
        assert!(!placement.use_tp_worker);
    }

    #[test]
    fn resident_slot_places_when_live_free_vram_cannot_cold_load() {
        let devices = [
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "RTX 3090".into(),
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
        let gpu_idle = vec!["cuda-0".to_string()];
        let loaded = model(18.0, 16.0);
        let cold =
            pick_placement(&plan, &gpu_idle, &loaded, 64, 2, &devices, false).expect("card size");
        assert_eq!(cold.slot_ids, vec!["cuda-0".to_string()]);
        let placement = pick_resident_placement(
            &plan,
            &idle,
            &["cuda-0".to_string()],
            &loaded,
            64,
            2,
            false,
            0,
        )
        .expect("resident slot is placeable");
        assert_eq!(placement.slot_ids, vec!["cuda-0".to_string()]);
        assert!(pick_resident_placement(
            &plan,
            &["cpu-0".to_string()],
            &["cuda-0".to_string()],
            &loaded,
            64,
            2,
            false,
            0
        )
        .is_none());
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

    #[test]
    fn qwen8b_skips_six_gb_card_when_ten_gb_is_idle() {
        let devices = mixed_1660_3080();
        let plan = build_compute_slots(&devices).unwrap();
        let idle: Vec<String> = plan.slots.iter().map(|s| s.id.clone()).collect();
        let placement =
            pick_placement(&plan, &idle, &model(4.0, 4.68), 32, 2, &devices, false).unwrap();
        assert_eq!(placement.slot_ids, vec!["cuda-1".to_string()]);
        assert!(!placement.use_tp_worker);
    }

    #[test]
    fn qwen8b_offloads_to_six_gb_when_ten_gb_is_busy() {
        let devices = mixed_1660_3080();
        let plan = build_compute_slots(&devices).unwrap();
        let idle = vec!["cuda-0".to_string(), "cpu-0".to_string()];
        let placement =
            pick_placement(&plan, &idle, &model(4.0, 4.68), 32, 2, &devices, false).unwrap();
        assert_eq!(placement.slot_ids, vec!["cuda-0".to_string()]);
        assert!(!placement.use_tp_worker);
    }

    #[test]
    fn qwen8b_does_not_cpu_overflow_when_gpus_exist() {
        let devices = mixed_1660_3080();
        let plan = build_compute_slots(&devices).unwrap();
        let idle = vec!["cpu-0".to_string()];
        assert!(
            pick_placement(&plan, &idle, &model(4.0, 4.68), 32, 2, &devices, false).is_none(),
            "crash-retry must not land 8B on cpu-0 while a GPU can still host it"
        );
    }

    #[test]
    fn preferred_cpu_pin_places_gpu_class_model_on_cpu() {
        let devices = mixed_1660_3080();
        let plan = build_compute_slots(&devices).unwrap();
        let idle = vec!["cpu-0".to_string()];
        let placement = pick_placement_with_cpu(
            &plan,
            &idle,
            &model(4.0, 4.68),
            32,
            2,
            &devices,
            false,
            16,
            32,
            &HashSet::new(),
            &HashMap::new(),
            true,
            0,
        )
        .expect("preferredSlotId=cpu-0 must honor the pin");
        assert_eq!(placement.slot_ids, vec!["cpu-0".to_string()]);
    }

    #[test]
    fn required_slot_placement_is_literal() {
        let devices = mixed_1660_3080();
        let plan = build_compute_slots(&devices).unwrap();
        let m = model(4.0, 4.68);
        let on_cpu = placement_for_required_slot(&plan, "cpu-0", &devices, &m, false, 0).unwrap();
        assert_eq!(on_cpu.slot_ids, vec!["cpu-0".to_string()]);
        let on_gpu = placement_for_required_slot(&plan, "cuda-0", &devices, &m, false, 0).unwrap();
        assert_eq!(on_gpu.slot_ids, vec!["cuda-0".to_string()]);
        assert!(placement_for_required_slot(&plan, "cuda-99", &devices, &m, false, 0).is_none());
    }

    fn twin_4090() -> [ComputeDevice; 3] {
        [
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
        ]
    }

    #[test]
    fn preferred_pin_in_tp_group_keeps_small_model_on_single_sibling() {
        let devices = twin_4090();
        let plan = build_compute_slots(&devices).unwrap();
        assert!(
            plan.slots
                .iter()
                .filter(|s| s.kind != "cpu")
                .all(|s| s.tp_group.is_some()),
            "matched 4090s must share a tp_group"
        );
        let small = model(8.0, 5.0);
        let placement =
            placement_for_required_slot(&plan, "cuda-0", &devices, &small, false, 0).unwrap();
        assert!(
            !placement.use_tp_worker,
            "small model that fits one card must not steal the TpGroup"
        );
        assert_eq!(placement.slot_ids, vec!["cuda-0".to_string()]);
    }

    #[test]
    fn preferred_pin_in_tp_group_expands_when_model_needs_pool() {
        let devices = twin_4090();
        let plan = build_compute_slots(&devices).unwrap();
        let large = model(40.0, 30.0);
        let placement =
            placement_for_required_slot(&plan, "cuda-1", &devices, &large, false, 0).unwrap();
        assert!(
            placement.use_tp_worker,
            "large model that cannot full-host on one card must expand to TP"
        );
        assert_eq!(placement.slot_ids.len(), 2);
        assert!(placement.slot_ids.contains(&"cuda-0".to_string()));
        assert!(placement.slot_ids.contains(&"cuda-1".to_string()));
    }

    #[test]
    fn dual_2gb_refuses_auto_cpu_for_gpu_class_eight_b() {
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
        // Auto path: do not silently crawl GPU-class models on cpu-0.
        assert!(pick_placement_with_cpu(
            &plan,
            &idle,
            &model(12.1, 5.0),
            16,
            2,
            &devices,
            false,
            16,
            16,
            &HashSet::new(),
            &HashMap::new(),
            false,
            0
        )
        .is_none());
        // Explicit preferredSlotId=cpu-* still places when RAM covers it.
        let pinned = pick_placement_with_cpu(
            &plan,
            &["cpu-0".to_string()],
            &model(12.1, 5.0),
            16,
            2,
            &devices,
            false,
            16,
            16,
            &HashSet::new(),
            &HashMap::new(),
            true,
            0,
        )
        .unwrap();
        assert_eq!(pinned.slot_ids, vec!["cpu-0".to_string()]);
        assert!(
            pick_placement_with_cpu(
                &plan,
                &["cpu-0".to_string()],
                &model(12.1, 5.0),
                6,
                2,
                &devices,
                false,
                16,
                6,
                &HashSet::new(),
                &HashMap::new(),
                true,
                0
            )
            .is_none(),
            "6 GB RAM must not start an 8B beside a 2 GB card"
        );
    }

    #[test]
    fn cpu_only_machine_still_places_on_cpu() {
        let devices = [ComputeDevice {
            id: "cpu:0".into(),
            kind: "cpu".into(),
            name: "CPU".into(),
            vram_gb: None,
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        }];
        let plan = build_compute_slots(&devices).unwrap();
        let idle: Vec<String> = plan.slots.iter().map(|s| s.id.clone()).collect();
        let placement = pick_placement_with_cpu(
            &plan,
            &idle,
            &model(4.0, 4.68),
            32,
            2,
            &devices,
            false,
            16,
            32,
            &HashSet::new(),
            &HashMap::new(),
            false,
            0,
        )
        .unwrap();
        assert_eq!(placement.slot_ids, vec!["cpu-0".to_string()]);
    }

    #[test]
    fn six_gb_only_machine_still_offloads() {
        let devices = [
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "GTX 1660 SUPER".into(),
                vram_gb: Some(6),
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
            pick_placement(&plan, &idle, &model(4.0, 4.68), 32, 2, &devices, false).unwrap();
        assert_eq!(placement.slot_ids, vec!["cuda-0".to_string()]);
    }

    #[test]
    fn eight_gb_cpu_slot_only_with_capable_cpu_for_coder_30b() {
        let devices = [
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "RTX 5050".into(),
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
        let plan = build_compute_slots(&devices).unwrap();
        let idle: Vec<String> = plan.slots.iter().map(|s| s.id.clone()).collect();
        // Auto path never parks 30B on cpu-0 while a GPU exists.
        assert!(pick_placement_with_cpu(
            &plan,
            &idle,
            &model(22.5, 19.0),
            31,
            2,
            &devices,
            false,
            32,
            31,
            &HashSet::new(),
            &HashMap::new(),
            false,
            0
        )
        .is_none());
        // preferredSlotId=cpu-* + capable CPU cores may still place.
        let on_cpu = pick_placement_with_cpu(
            &plan,
            &["cpu-0".to_string()],
            &model(22.5, 19.0),
            31,
            2,
            &devices,
            false,
            32,
            31,
            &HashSet::new(),
            &HashMap::new(),
            true,
            0,
        )
        .unwrap();
        assert_eq!(on_cpu.slot_ids, vec!["cpu-0".to_string()]);
        assert!(
            pick_placement_with_cpu(
                &plan,
                &["cpu-0".to_string()],
                &model(22.5, 19.0),
                20,
                2,
                &devices,
                false,
                32,
                20,
                &HashSet::new(),
                &HashMap::new(),
                true,
                0
            )
            .is_none(),
            "20 GB RAM must not start a 19 GB coder"
        );
        assert!(
            pick_placement_with_cpu(
                &plan,
                &["cpu-0".to_string()],
                &model(22.5, 19.0),
                31,
                2,
                &devices,
                false,
                8,
                31,
                &HashSet::new(),
                &HashMap::new(),
                true,
                0
            )
            .is_none(),
            "consumer cores: do not crawl 30B on CPU just because RAM fits"
        );
    }

    #[test]
    fn four_gb_places_eight_b_with_ram_offload() {
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
        let placement = pick_placement_with_cpu(
            &plan,
            &idle,
            &model(10.7, 5.0),
            16,
            2,
            &devices,
            false,
            16,
            16,
            &HashSet::new(),
            &HashMap::new(),
            false,
            0,
        )
        .unwrap();
        assert_eq!(placement.slot_ids, vec!["cuda-0".to_string()]);
        let fourteen = pick_placement_with_cpu(
            &plan,
            &idle,
            &model(13.2, 9.0),
            16,
            2,
            &devices,
            false,
            16,
            16,
            &HashSet::new(),
            &HashMap::new(),
            false,
            0,
        )
        .unwrap();
        assert_eq!(
            fourteen.slot_ids,
            vec!["cpu-0".to_string()],
            "4 GB card must not layer-offload a 14B; capable CPU+RAM can still run it"
        );
        assert!(
            pick_placement_with_cpu(
                &plan,
                &idle,
                &model(13.2, 9.0),
                8,
                2,
                &devices,
                false,
                16,
                8,
                &HashSet::new(),
                &HashMap::new(),
                false,
                0
            )
            .is_none(),
            "8 GB RAM must not start a 14B"
        );
        assert!(
            pick_placement_with_cpu(
                &plan,
                &idle,
                &model(13.2, 9.0),
                16,
                2,
                &devices,
                false,
                4,
                16,
                &HashSet::new(),
                &HashMap::new(),
                false,
                0
            )
            .is_none(),
            "weak CPU must not claim a 14B even with RAM"
        );
    }

    fn image_model(min_vram: f64) -> CatalogModel {
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
    fn image_job_uses_smallest_cuda_slot_that_meets_vram() {
        let devices = mixed_1660_3080();
        let plan = build_compute_slots(&devices).unwrap();
        let idle: Vec<String> = plan.slots.iter().map(|s| s.id.clone()).collect();
        let placement =
            pick_placement(&plan, &idle, &image_model(8.0), 64, 2, &devices, false).unwrap();
        assert_eq!(placement.slot_ids, vec!["cuda-1".to_string()]);
        assert!(!placement.use_tp_worker);
    }

    #[test]
    fn image_job_skips_cpu_and_undersized_gpu() {
        let devices = mixed_1660_3080();
        let plan = build_compute_slots(&devices).unwrap();
        let idle: Vec<String> = plan.slots.iter().map(|s| s.id.clone()).collect();
        assert!(pick_placement(&plan, &idle, &image_model(24.0), 64, 2, &devices, false).is_none());
    }

    #[test]
    fn reclaimable_warm_uses_nameplate_not_live_free() {
        // Idle cuda-0 nameplate-hosts; with reclaimable set we place even though
        // live free would look too small (our warm Ornith is occupying it).
        let devices = [
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "RTX 3050 Ti".into(),
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
        let plan = build_compute_slots(&devices).unwrap();
        let idle = vec!["cuda-0".to_string()];
        let reclaimable = HashSet::from(["cuda-0".to_string()]);
        let m = model(10.7, 5.0); // ~8B class with RAM offload on 4 GB
        let placement = pick_placement_with_cpu(
            &plan,
            &idle,
            &m,
            15,
            2,
            &devices,
            false,
            16,
            15,
            &reclaimable,
            &HashMap::new(),
            false,
            0,
        );
        assert!(
            placement.is_some(),
            "warm resident on idle slot must be reclaimable for cold load"
        );
        assert_eq!(placement.unwrap().slot_ids, vec!["cuda-0".to_string()]);
    }

    #[test]
    fn reclaimable_warm_sys_ram_credit_allows_offload() {
        let devices = [
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "Laptop 3050 Ti".into(),
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
        let plan = build_compute_slots(&devices).unwrap();
        let idle = vec!["cuda-0".to_string()];
        let reclaimable = HashSet::from(["cuda-0".to_string()]);
        let m = model(3.9, 5.5);
        // Live free RAM looks too small for KV/spill — but warm eviction frees ~6 GB.
        assert!(
            pick_placement_with_cpu(
                &plan,
                &idle,
                &m,
                15,
                2,
                &devices,
                false,
                16,
                1,
                &reclaimable,
                &HashMap::new(),
                false,
                0
            )
            .is_none(),
            "without reclaim credit, tight sys RAM must refuse"
        );
        let credit = HashMap::from([("cuda-0".to_string(), 6u32)]);
        let placement = pick_placement_with_cpu(
            &plan,
            &idle,
            &m,
            15,
            2,
            &devices,
            false,
            16,
            1,
            &reclaimable,
            &credit,
            false,
            0,
        );
        assert!(
            placement.is_some(),
            "evicting warm resident must credit sys RAM for the cold offload"
        );
        let detail = placement_miss_detail(
            &plan,
            &idle,
            &m,
            false,
            &reclaimable,
            1,
            &HashMap::new(),
            16,
            2,
            0,
        );
        assert_eq!(
            detail.code,
            crate::invoke_code::InvokeErrorCode::InsufficientVram
        );
        assert!(
            detail.detail.contains("system RAM"),
            "miss detail must blame sys RAM when VRAM looks fine: {}",
            detail.detail
        );
    }

    #[test]
    fn image_job_busy_fitting_gpu_reports_agent_busy() {
        let devices = mixed_1660_3080();
        let plan = build_compute_slots(&devices).unwrap();
        // 10 GB card busy; 6 GB idle cannot host an 8 GB picture floor.
        let idle: Vec<String> = plan
            .slots
            .iter()
            .filter(|s| s.id != "cuda-1")
            .map(|s| s.id.clone())
            .collect();
        let model = image_model(8.0);
        assert!(pick_placement(&plan, &idle, &model, 64, 2, &devices, false).is_none());
        let detail = placement_miss_detail(
            &plan,
            &idle,
            &model,
            false,
            &HashSet::new(),
            16,
            &HashMap::new(),
            64,
            2,
            0,
        );
        assert_eq!(detail.code, crate::invoke_code::InvokeErrorCode::AgentBusy);
        assert!(
            detail.detail.contains("waiting for a GPU"),
            "want wait-for-fit, got {}",
            detail.detail
        );
    }

    #[test]
    fn sjvl_busy_cuda_does_not_auto_pick_igpu_reports_agent_busy() {
        // Fleet: DESKTOP-SJVL4OL — RTX busy, AMD "Radeon Graphics" ~2 GB idle.
        // iGPU stays in the plan (server may pin preferredSlotId=igpu-0); local
        // auto-pick and miss detail must not treat it as the capacity answer.
        use crate::compute_pool::{ComputePlan, ComputeSlot, PoolDevice, VirtualCard};
        let devices = [
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "NVIDIA GeForce RTX 5050".into(),
                vram_gb: Some(8),
                vram_used_gb: Some(7),
                util_pct: Some(95),
                enabled: true,
            },
            ComputeDevice {
                id: "amd:0".into(),
                kind: "integrated".into(),
                name: "AMD Radeon(TM) Graphics".into(),
                vram_gb: Some(2),
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
        let cuda_card = VirtualCard {
            devices: vec![PoolDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "NVIDIA GeForce RTX 5050".into(),
                vram_gb: 8,
                cuda_index: Some(0),
            }],
            strategy: PoolStrategy::Single,
            display_name: "NVIDIA GeForce RTX 5050".into(),
            total_vram_gb: 8,
            tensor_split: vec![],
            cuda_device_ids: vec![0],
            uses_vulkan: false,
            gpu_layer_budget: 0,
        };
        let igpu_card = VirtualCard {
            devices: vec![PoolDevice {
                id: "amd:0".into(),
                kind: "integrated".into(),
                name: "AMD Radeon(TM) Graphics".into(),
                vram_gb: 2,
                cuda_index: None,
            }],
            strategy: PoolStrategy::Vulkan,
            display_name: "AMD Radeon(TM) Graphics".into(),
            total_vram_gb: 2,
            tensor_split: vec![],
            cuda_device_ids: vec![],
            uses_vulkan: true,
            gpu_layer_budget: 0,
        };
        let cpu_card = VirtualCard {
            devices: vec![PoolDevice {
                id: "cpu:0".into(),
                kind: "cpu".into(),
                name: "CPU".into(),
                vram_gb: 0,
                cuda_index: None,
            }],
            strategy: PoolStrategy::CpuOnly,
            display_name: "CPU".into(),
            total_vram_gb: 0,
            tensor_split: vec![],
            cuda_device_ids: vec![],
            uses_vulkan: false,
            gpu_layer_budget: 0,
        };
        let plan = ComputePlan {
            slots: vec![
                ComputeSlot {
                    id: "cuda-0".into(),
                    kind: "discrete_cuda".into(),
                    priority: 10,
                    card: cuda_card,
                    cuda_visible: vec![0],
                    tp_group: None,
                },
                ComputeSlot {
                    id: "igpu-0".into(),
                    kind: "integrated".into(),
                    priority: 40,
                    card: igpu_card,
                    cuda_visible: vec![],
                    tp_group: None,
                },
                ComputeSlot {
                    id: "cpu-0".into(),
                    kind: "cpu".into(),
                    priority: 100,
                    card: cpu_card,
                    cuda_visible: vec![],
                    tp_group: None,
                },
            ],
            tp_groups: Default::default(),
        };
        let pin =
            placement_for_required_slot(&plan, "igpu-0", &devices, &model(6.0, 5.0), false, 0)
                .unwrap();
        assert_eq!(pin.slot_ids, vec!["igpu-0".to_string()]);
        let idle = vec!["igpu-0".to_string(), "cpu-0".to_string()];
        let m = model(6.0, 5.0);
        assert!(
            pick_placement_with_cpu(
                &plan,
                &idle,
                &m,
                32,
                2,
                &devices,
                false,
                16,
                24,
                &HashSet::new(),
                &HashMap::new(),
                false,
                0
            )
            .is_none(),
            "auto-pick must not land on soft-estimate iGPU / CPU while discrete nameplate-hosts"
        );
        let detail = placement_miss_detail(
            &plan,
            &idle,
            &m,
            false,
            &HashSet::new(),
            24,
            &HashMap::new(),
            32,
            2,
            0,
        );
        assert_eq!(detail.code, crate::invoke_code::InvokeErrorCode::AgentBusy);
        assert!(
            detail.detail.contains("waiting for a GPU"),
            "want hop-on-busy, got {}",
            detail.detail
        );
    }

    #[test]
    fn image_job_places_on_metal_slot() {
        use crate::compute_pool::{ComputePlan, ComputeSlot, PoolDevice, VirtualCard};
        let card = VirtualCard {
            devices: vec![PoolDevice {
                id: "metal:0".into(),
                kind: "metal".into(),
                name: "Apple M4".into(),
                vram_gb: 24,
                cuda_index: None,
            }],
            strategy: PoolStrategy::Metal,
            display_name: "Apple M4".into(),
            total_vram_gb: 24,
            tensor_split: vec![],
            cuda_device_ids: vec![],
            uses_vulkan: false,
            gpu_layer_budget: 0,
        };
        let plan = ComputePlan {
            slots: vec![ComputeSlot {
                id: "metal-0".into(),
                kind: "metal".into(),
                priority: 15,
                card,
                cuda_visible: vec![],
                tp_group: None,
            }],
            tp_groups: Default::default(),
        };
        let idle = vec!["metal-0".to_string()];
        let placement =
            pick_placement(&plan, &idle, &image_model(16.0), 32, 2, &[], false).unwrap();
        assert_eq!(placement.slot_ids, vec!["metal-0".to_string()]);
        assert!(placement.cuda_visible.is_empty());
    }

    #[test]
    fn image_job_places_on_amd_vulkan_slot() {
        use crate::compute_pool::{ComputePlan, ComputeSlot, PoolDevice, VirtualCard};
        let card = VirtualCard {
            devices: vec![PoolDevice {
                id: "amd:1".into(),
                kind: "discrete".into(),
                name: "AMD Radeon RX 7900 XTX".into(),
                vram_gb: 24,
                cuda_index: None,
            }],
            strategy: PoolStrategy::Vulkan,
            display_name: "AMD Radeon RX 7900 XTX".into(),
            total_vram_gb: 24,
            tensor_split: vec![],
            cuda_device_ids: vec![],
            uses_vulkan: true,
            gpu_layer_budget: 0,
        };
        let plan = ComputePlan {
            slots: vec![ComputeSlot {
                id: "vulkan-0".into(),
                kind: "discrete_vulkan".into(),
                priority: 20,
                card,
                cuda_visible: vec![],
                tp_group: None,
            }],
            tp_groups: Default::default(),
        };
        let idle = vec!["vulkan-0".to_string()];
        let placement =
            pick_placement(&plan, &idle, &image_model(16.0), 32, 2, &[], false).unwrap();
        assert_eq!(placement.slot_ids, vec!["vulkan-0".to_string()]);
        assert_eq!(placement.cuda_visible, vec![1]);
    }

    #[test]
    fn image_job_places_on_intel_arc_slot() {
        use crate::compute_pool::{ComputePlan, ComputeSlot, PoolDevice, VirtualCard};
        let card = VirtualCard {
            devices: vec![PoolDevice {
                id: "pci-intel:0".into(),
                kind: "discrete".into(),
                name: "Intel Arc A770".into(),
                vram_gb: 16,
                cuda_index: None,
            }],
            strategy: PoolStrategy::Vulkan,
            display_name: "Intel Arc A770".into(),
            total_vram_gb: 16,
            tensor_split: vec![],
            cuda_device_ids: vec![],
            uses_vulkan: true,
            gpu_layer_budget: 0,
        };
        let plan = ComputePlan {
            slots: vec![ComputeSlot {
                id: "vulkan-0".into(),
                kind: "discrete_vulkan".into(),
                priority: 20,
                card,
                cuda_visible: vec![],
                tp_group: None,
            }],
            tp_groups: Default::default(),
        };
        let idle = vec!["vulkan-0".to_string()];
        let placement =
            pick_placement(&plan, &idle, &image_model(12.0), 32, 2, &[], false).unwrap();
        assert_eq!(placement.slot_ids, vec!["vulkan-0".to_string()]);
        assert_eq!(placement.cuda_visible, vec![0]);
    }

    fn mixed_1660_3080() -> [ComputeDevice; 3] {
        [
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "GTX 1660 SUPER".into(),
                vram_gb: Some(6),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "nvidia:1".into(),
                kind: "discrete".into(),
                name: "RTX 3080".into(),
                vram_gb: Some(10),
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
        ]
    }
}
