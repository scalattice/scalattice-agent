//! Dual-use occupancy: skip a GPU when foreign VRAM leaves no room for our models.
//!
//! Not a fault. Warm Scalattice weights (slot `loaded_models`) stay available.
//! nvidia-smi can lag after our own evict/load — ignore that window and require
//! a hold before latching. Leftover usage that still hosts the smallest
//! advertised SKU is ignored (a couple GB of desktop on a 24 GB card is fine).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::compute_pool::{ComputeSlot, PoolStrategy};
use crate::models::{can_host_model, occupancy_min_vram_gb};
use crate::protocol::CatalogModel;

/// Driver / Windows desktop / compositor. Not enough to take the box out.
pub const OS_SLACK_GB: f64 = 1.5;
/// Two heartbeats (~12s) of occupied samples before we skip the slot.
pub const ENTER_HOLD: Duration = Duration::from_secs(12);
/// Come back after free samples hold — smi can bounce while a process exits.
pub const EXIT_HOLD: Duration = Duration::from_secs(8);
/// After our evict/load/job, smi often still shows full. Do not enter occupied.
pub const IGNORE_AFTER_OURS: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct SlotOccupancyView {
    pub slot_id: String,
    pub kind: String,
    pub strategy: PoolStrategy,
    pub worker_busy: bool,
    pub loaded_models: Vec<String>,
    pub live_free_gb: Option<f64>,
    pub min_need_gb: Option<f64>,
}

#[derive(Debug, Default)]
pub struct OccupancyWatch {
    latched: HashSet<String>,
    candidate_since: HashMap<String, Instant>,
    clear_since: HashMap<String, Instant>,
    ignore_until: Option<Instant>,
}

impl OccupancyWatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn latched_ids(&self) -> HashSet<String> {
        self.latched.clone()
    }

    pub fn is_latched(&self, slot_id: &str) -> bool {
        self.latched.contains(slot_id)
    }

    pub fn note_our_vram(&mut self, now: Instant) {
        self.ignore_until = Some(now + IGNORE_AFTER_OURS);
        self.candidate_since.clear();
    }

    pub fn update(&mut self, now: Instant, views: &[SlotOccupancyView]) -> HashSet<String> {
        let ignoring = self
            .ignore_until
            .map(|until| now < until)
            .unwrap_or(false);
        let seen: HashSet<String> = views.iter().map(|v| v.slot_id.clone()).collect();

        for view in views {
            let raw = raw_foreign_occupancy(view);
            if raw && !ignoring {
                self.clear_since.remove(&view.slot_id);
                let since = *self
                    .candidate_since
                    .entry(view.slot_id.clone())
                    .or_insert(now);
                if now.duration_since(since) >= ENTER_HOLD {
                    self.latched.insert(view.slot_id.clone());
                }
            } else {
                self.candidate_since.remove(&view.slot_id);
                if raw && ignoring {
                    continue;
                }
                if !self.latched.contains(&view.slot_id) {
                    self.clear_since.remove(&view.slot_id);
                    continue;
                }
                let since = *self.clear_since.entry(view.slot_id.clone()).or_insert(now);
                if now.duration_since(since) >= EXIT_HOLD {
                    self.latched.remove(&view.slot_id);
                    self.clear_since.remove(&view.slot_id);
                }
            }
        }

        self.latched.retain(|id| seen.contains(id));
        self.candidate_since.retain(|id, _| seen.contains(id));
        self.clear_since.retain(|id, _| seen.contains(id));
        self.latched.clone()
    }
}

/// True when this idle GPU has no Scalattice weights and live free VRAM cannot
/// host the smallest advertised model that this card could otherwise run.
pub fn raw_foreign_occupancy(view: &SlotOccupancyView) -> bool {
    if view.kind.eq_ignore_ascii_case("cpu") {
        return false;
    }
    if matches!(view.strategy, PoolStrategy::CpuOnly | PoolStrategy::Metal) {
        return false;
    }
    if view.worker_busy {
        return false;
    }
    if !view.loaded_models.is_empty() {
        return false;
    }
    let Some(min_need) = view.min_need_gb.filter(|v| *v > 0.0) else {
        return false;
    };
    let Some(free) = view.live_free_gb else {
        return false;
    };
    free + OS_SLACK_GB + 0.005 < min_need
}

pub fn slot_live_free_gb(
    slot: &ComputeSlot,
    live_cuda: &std::collections::HashMap<u32, f64>,
) -> Option<f64> {
    match slot.card.strategy {
        PoolStrategy::Single | PoolStrategy::TensorParallel => {
            if slot.cuda_visible.is_empty() {
                return None;
            }
            slot.cuda_visible
                .iter()
                .filter_map(|idx| live_cuda.get(idx).copied())
                .reduce(f64::min)
        }
        PoolStrategy::Vulkan => {
            let index = slot.card.devices.iter().find_map(|device| {
                device
                    .id
                    .strip_prefix("amd:")
                    .and_then(|s| s.parse::<usize>().ok())
            });
            crate::specs::live_rocm_free_vram_gb(index)
        }
        PoolStrategy::Metal | PoolStrategy::CpuOnly => None,
    }
}

pub fn smallest_advertised_need_gb(
    card: &crate::compute_pool::VirtualCard,
    catalog: &[CatalogModel],
    advertised: &[String],
    ram_gb: u32,
    cpu_ram_headroom_gb: u32,
) -> Option<f64> {
    if advertised.is_empty() || catalog.is_empty() {
        return None;
    }
    let mut min_need: Option<f64> = None;
    for model in catalog {
        if !advertised
            .iter()
            .any(|id| id.eq_ignore_ascii_case(&model.model_id))
        {
            continue;
        }
        if !can_host_model(model, card, ram_gb, cpu_ram_headroom_gb) {
            continue;
        }
        let need = occupancy_min_vram_gb(model);
        min_need = Some(match min_need {
            Some(existing) => existing.min(need),
            None => need,
        });
    }
    min_need
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(free: Option<f64>, need: Option<f64>, loaded: &[&str], busy: bool) -> SlotOccupancyView {
        SlotOccupancyView {
            slot_id: "cuda-0".into(),
            kind: "discrete_cuda".into(),
            strategy: PoolStrategy::Single,
            worker_busy: busy,
            loaded_models: loaded.iter().map(|s| (*s).to_string()).collect(),
            live_free_gb: free,
            min_need_gb: need,
        }
    }

    #[test]
    fn leftover_desktop_vram_is_not_occupancy() {
        // 2 GB used on a 24 GB card, 8B still needs ~2.5 GB at the offload floor.
        assert!(!raw_foreign_occupancy(&view(Some(22.0), Some(2.5), &[], false)));
        // 27B-only box: 8 GB still placeable after slack.
        assert!(!raw_foreign_occupancy(&view(Some(8.0), Some(8.0), &[], false)));
    }

    #[test]
    fn full_foreign_vram_is_occupancy() {
        assert!(raw_foreign_occupancy(&view(Some(0.4), Some(8.0), &[], false)));
        assert!(raw_foreign_occupancy(&view(Some(0.0), Some(2.5), &[], false)));
    }

    #[test]
    fn our_warm_weights_are_not_occupancy() {
        assert!(!raw_foreign_occupancy(&view(
            Some(0.3),
            Some(8.0),
            &["qwen-3.8-27b"],
            false
        )));
    }

    #[test]
    fn missing_smi_is_not_occupancy() {
        assert!(!raw_foreign_occupancy(&view(None, Some(8.0), &[], false)));
    }

    #[test]
    fn latch_requires_hold_and_ignores_our_vram_lag() {
        let t0 = Instant::now();
        let mut watch = OccupancyWatch::new();
        let occupied = view(Some(0.2), Some(8.0), &[], false);

        watch.update(t0, &[occupied.clone()]);
        assert!(watch.latched_ids().is_empty());

        watch.update(t0 + Duration::from_secs(11), &[occupied.clone()]);
        assert!(watch.latched_ids().is_empty());

        watch.update(t0 + ENTER_HOLD, &[occupied.clone()]);
        assert!(watch.is_latched("cuda-0"));

        let mut fresh = OccupancyWatch::new();
        fresh.note_our_vram(t0);
        fresh.update(t0 + Duration::from_secs(5), &[occupied.clone()]);
        assert!(fresh.latched_ids().is_empty());
        // Ignore window ended: start the enter hold from the first post-ignore sample.
        fresh.update(t0 + IGNORE_AFTER_OURS, &[occupied.clone()]);
        assert!(fresh.latched_ids().is_empty());
        fresh.update(t0 + IGNORE_AFTER_OURS + ENTER_HOLD, &[occupied]);
        assert!(fresh.is_latched("cuda-0"));
    }

    #[test]
    fn unlatches_after_free_hold() {
        let t0 = Instant::now();
        let mut watch = OccupancyWatch::new();
        let occupied = view(Some(0.2), Some(8.0), &[], false);
        watch.update(t0, &[occupied.clone()]);
        watch.update(t0 + ENTER_HOLD, &[occupied]);
        assert!(watch.is_latched("cuda-0"));

        let free = view(Some(20.0), Some(8.0), &[], false);
        watch.update(t0 + ENTER_HOLD + Duration::from_secs(1), &[free.clone()]);
        assert!(watch.is_latched("cuda-0"));
        watch.update(t0 + ENTER_HOLD + EXIT_HOLD + Duration::from_secs(1), &[free]);
        assert!(!watch.is_latched("cuda-0"));
    }
}
