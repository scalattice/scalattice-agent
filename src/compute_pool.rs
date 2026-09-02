use crate::specs::ComputeDevice;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::warn;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PoolDevice {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub vram_gb: u32,
    pub cuda_index: Option<u32>,
}

/// How the pool prefers to run when accelerators allow a full fit.
///
/// Partial GPU↔CPU offload is a cascade fallback (see `load_param_candidates`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolStrategy {
    /// One NVIDIA CUDA device; try all layers on GPU first.
    Single,
    /// All enabled NVIDIA CUDA devices as one accelerator (tensor split by VRAM).
    TensorParallel,
    /// Enabled AMD/Intel GPUs via llama.cpp Vulkan (no CUDA device pin).
    Vulkan,
    /// Apple Silicon unified GPU via llama.cpp Metal.
    Metal,
    CpuOnly,
}

#[derive(Debug, Clone)]
pub struct VirtualCard {
    pub devices: Vec<PoolDevice>,
    pub strategy: PoolStrategy,
    pub display_name: String,
    pub total_vram_gb: u32,
    /// Fraction of model tensors per CUDA device (sums to 1.0). Empty unless CUDA TP.
    #[cfg_attr(not(test), allow(dead_code))]
    pub tensor_split: Vec<f32>,
    pub cuda_device_ids: Vec<u32>,
    /// True when strategy is Vulkan (AMD/Intel enabled, no CUDA in the pool).
    pub uses_vulkan: bool,
    /// Conservative layer count for offload *fallback* tiers after full-GPU OOM.
    pub gpu_layer_budget: u32,
}

/// Must run **before** `init_backend()` when the supervisor still hosts llama in-process.
/// Mixed NVIDIA gens (e.g. 1650 Super + 1050 Ti) make llama.cpp CUDA abort even on
/// "single device" loads while both cards stay visible. Prefer per-slot workers with
/// `apply_slot_cuda_visibility` instead; this remains for legacy single-process paths.
#[allow(dead_code)]
pub fn restrict_heterogeneous_cuda_visibility(devices: &[ComputeDevice]) {
    use std::sync::Once;
    static WARNED: Once = Once::new();

    let mut cuda: Vec<(u32, String, u32)> = Vec::new();
    for device in devices.iter().filter(|d| d.enabled) {
        let Some(idx) = parse_cuda_index(&device.id) else {
            continue;
        };
        if device.kind != "discrete" {
            continue;
        }
        cuda.push((idx, device.name.clone(), effective_vram_gb(device)));
    }
    if cuda.len() < 2 {
        return;
    }
    let names: Vec<String> = cuda.iter().map(|c| c.1.clone()).collect();
    let vrams: Vec<u32> = cuda.iter().map(|c| c.2).collect();
    if cuda_name_vram_homogeneous(&names, &vrams) {
        return;
    }

    let primary_pos = pick_primary_cuda_pos(
        &cuda.iter().map(|c| c.0).collect::<Vec<_>>(),
        &vrams,
    );
    let (physical, name, vram) = &cuda[primary_pos];
    // SAFETY: must be set before the first CUDA / llama.cpp backend init in this process.
    pin_cuda_indices_to_pci_bus();
    std::env::set_var("CUDA_VISIBLE_DEVICES", physical.to_string());
    WARNED.call_once(|| {
        warn!(
            kept_cuda_index = physical,
            kept_name = %name,
            kept_vram_gb = vram,
            hidden_gpus = cuda.len() - 1,
            "mixed NVIDIA GPUs: CUDA_VISIBLE_DEVICES limited to the largest card so llama.cpp cannot abort on multi-arch init"
        );
    });
}

fn cuda_visibility_remapped_to_zero(physical: u32) -> bool {
    let Ok(raw) = std::env::var("CUDA_VISIBLE_DEVICES") else {
        return false;
    };
    let parts: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    parts.len() == 1 && parts[0].parse::<u32>().ok() == Some(physical)
}

/// Normalize NVIDIA marketing names so "NVIDIA GeForce RTX 4090" == "rtx 4090" family match.
pub fn normalize_cuda_sku(name: &str) -> String {
    let mut s = name.trim().to_ascii_lowercase();
    for prefix in [
        "nvidia ",
        "geforce ",
        "tesla ",
        "quadro ",
        "rtx ",
        "gtx ",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim_start().to_string();
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn cuda_name_vram_homogeneous(names: &[String], vrams: &[u32]) -> bool {
    if names.len() < 2 || names.len() != vrams.len() {
        return false;
    }
    let skus: Vec<String> = names.iter().map(|n| normalize_cuda_sku(n)).collect();
    if skus.iter().any(|s| s.is_empty()) || !skus.iter().all(|s| s == &skus[0]) {
        return false;
    }
    let min_v = *vrams.iter().min().unwrap_or(&0);
    let max_v = *vrams.iter().max().unwrap_or(&0);
    max_v.saturating_sub(min_v) <= 1
}

fn pick_primary_cuda_pos(ids: &[u32], vrams: &[u32]) -> usize {
    (0..ids.len())
        .max_by(|&i, &j| {
            vrams[i]
                .cmp(&vrams[j])
                // On equal VRAM prefer the lower CUDA index (usually the primary slot).
                .then_with(|| ids[j].cmp(&ids[i]))
        })
        .unwrap_or(0)
}

/// Schedulable compute unit: one accelerator (or CPU) the hypervisor can assign a job to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeSlot {
    pub id: String,
    /// discrete_cuda | discrete_vulkan | integrated | cpu
    pub kind: String,
    pub priority: u32,
    pub card: VirtualCard,
    /// Physical CUDA indices this slot owns (for CVD). Empty for Vulkan/CPU.
    pub cuda_visible: Vec<u32>,
    /// Homogeneous CUDA siblings that can be claimed together for tensor-parallel.
    pub tp_group: Option<String>,
}

/// Machine-wide slot plan: independent workers plus optional TP groups.
#[derive(Debug, Clone)]
pub struct ComputePlan {
    pub slots: Vec<ComputeSlot>,
    /// group_id → physical CUDA indices (homogeneous only).
    pub tp_groups: HashMap<String, Vec<u32>>,
}

impl PoolStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            PoolStrategy::Single => "single",
            PoolStrategy::TensorParallel => "tensor_parallel",
            PoolStrategy::Vulkan => "vulkan",
            PoolStrategy::Metal => "metal",
            PoolStrategy::CpuOnly => "cpu",
        }
    }
}

impl Serialize for VirtualCard {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("VirtualCard", 8)?;
        state.serialize_field("devices", &self.devices)?;
        state.serialize_field("strategy", self.strategy.as_str())?;
        state.serialize_field("display_name", &self.display_name)?;
        state.serialize_field("total_vram_gb", &self.total_vram_gb)?;
        state.serialize_field("tensor_split", &self.tensor_split)?;
        state.serialize_field("cuda_device_ids", &self.cuda_device_ids)?;
        state.serialize_field("uses_vulkan", &self.uses_vulkan)?;
        state.serialize_field("gpu_layer_budget", &self.gpu_layer_budget)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for VirtualCard {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            devices: Vec<PoolDevice>,
            strategy: String,
            display_name: String,
            total_vram_gb: u32,
            #[serde(default)]
            tensor_split: Vec<f32>,
            #[serde(default)]
            cuda_device_ids: Vec<u32>,
            #[serde(default)]
            uses_vulkan: bool,
            #[serde(default)]
            gpu_layer_budget: u32,
        }
        let raw = Raw::deserialize(deserializer)?;
        let strategy = match raw.strategy.as_str() {
            "tensor_parallel" => PoolStrategy::TensorParallel,
            "vulkan" => PoolStrategy::Vulkan,
            "metal" => PoolStrategy::Metal,
            "cpu" => PoolStrategy::CpuOnly,
            _ => PoolStrategy::Single,
        };
        Ok(VirtualCard {
            devices: raw.devices,
            strategy,
            display_name: raw.display_name,
            total_vram_gb: raw.total_vram_gb,
            tensor_split: raw.tensor_split,
            cuda_device_ids: raw.cuda_device_ids,
            uses_vulkan: raw.uses_vulkan,
            gpu_layer_budget: raw.gpu_layer_budget,
        })
    }
}

impl Serialize for PoolDevice {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("PoolDevice", 5)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("vram_gb", &self.vram_gb)?;
        state.serialize_field("cuda_index", &self.cuda_index)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for PoolDevice {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            id: String,
            kind: String,
            name: String,
            vram_gb: u32,
            #[serde(default)]
            cuda_index: Option<u32>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(PoolDevice {
            id: raw.id,
            kind: raw.kind,
            name: raw.name,
            vram_gb: raw.vram_gb,
            cuda_index: raw.cuda_index,
        })
    }
}

fn card_for_devices(
    pool_devices: Vec<PoolDevice>,
    strategy: PoolStrategy,
    total_vram_gb: u32,
    display_name: String,
    tensor_split: Vec<f32>,
    cuda_device_ids: Vec<u32>,
    uses_vulkan: bool,
) -> VirtualCard {
    let budget_vram = if strategy == PoolStrategy::CpuOnly {
        0
    } else if !cuda_device_ids.is_empty() {
        pool_devices
            .iter()
            .filter(|d| d.cuda_index.is_some())
            .map(|d| d.vram_gb)
            .max()
            .unwrap_or(total_vram_gb.max(1))
    } else {
        total_vram_gb.max(1)
    };
    let gpu_layer_budget = match strategy {
        PoolStrategy::CpuOnly => 0,
        _ => offload_layer_budget(budget_vram),
    };
    VirtualCard {
        devices: pool_devices,
        strategy,
        display_name,
        total_vram_gb,
        tensor_split,
        cuda_device_ids,
        uses_vulkan,
        gpu_layer_budget,
    }
}

/// Partition enabled devices into independent compute slots.
///
/// - Each discrete NVIDIA GPU → its own Single slot (CVD pin in the worker).
/// - Homogeneous NVIDIA siblings share a `tp_group` so large models can claim them together.
/// - Each AMD/Intel discrete / iGPU → Vulkan slot (when feature enabled).
/// - CPU → always present as overflow.
pub fn build_compute_slots(devices: &[ComputeDevice]) -> Result<ComputePlan> {
    let enabled: Vec<&ComputeDevice> = devices.iter().filter(|d| d.enabled).collect();
    if enabled.is_empty() {
        bail!("no compute devices enabled");
    }

    let mut cuda: Vec<(&ComputeDevice, u32, u32)> = Vec::new(); // device, index, vram
    let mut vulkan_discrete: Vec<&ComputeDevice> = Vec::new();
    let mut metal: Vec<&ComputeDevice> = Vec::new();
    let mut integrated: Vec<&ComputeDevice> = Vec::new();
    let mut cpu: Option<&ComputeDevice> = None;

    for device in &enabled {
        if is_metal_accelerator(device) {
            metal.push(*device);
            continue;
        }
        if let Some(idx) = parse_cuda_index(&device.id) {
            if device.kind == "discrete" {
                cuda.push((*device, idx, effective_vram_gb(device)));
                continue;
            }
        }
        if device.kind == "cpu" {
            cpu = Some(*device);
            continue;
        }
        if device.kind == "integrated" {
            integrated.push(*device);
            continue;
        }
        if is_vulkan_accelerator(device) {
            vulkan_discrete.push(*device);
        }
    }

    let mut slots = Vec::new();
    let mut tp_groups: HashMap<String, Vec<u32>> = HashMap::new();

    let cuda_homogeneous = {
        let names: Vec<String> = cuda.iter().map(|(d, _, _)| d.name.clone()).collect();
        let vrams: Vec<u32> = cuda.iter().map(|(_, _, v)| *v).collect();
        cuda.len() >= 2 && cuda_name_vram_homogeneous(&names, &vrams)
    };
    let tp_group_id = if cuda_homogeneous {
        Some("cuda-tp-0".to_string())
    } else {
        None
    };
    if let Some(ref gid) = tp_group_id {
        tp_groups.insert(gid.clone(), cuda.iter().map(|(_, idx, _)| *idx).collect());
    }

    // Prefer larger VRAM first for stable slot ordering / primary pick.
    let mut cuda_sorted = cuda.clone();
    cuda_sorted.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));

    for (device, idx, vram) in &cuda_sorted {
        let pool_dev = PoolDevice {
            id: device.id.clone(),
            kind: device.kind.clone(),
            name: device.name.clone(),
            vram_gb: *vram,
            cuda_index: Some(*idx),
        };
        // Worker remaps CVD so llama always sees device 0.
        let card = card_for_devices(
            vec![pool_dev],
            PoolStrategy::Single,
            *vram,
            device.name.clone(),
            Vec::new(),
            vec![0],
            false,
        );
        slots.push(ComputeSlot {
            id: format!("cuda-{idx}"),
            kind: "discrete_cuda".into(),
            priority: 10,
            card,
            cuda_visible: vec![*idx],
            tp_group: tp_group_id.clone(),
        });
    }

    if metal_runtime_supported() {
        for (i, device) in metal.iter().enumerate() {
            let vram = effective_vram_gb(device);
            let pool_dev = PoolDevice {
                id: device.id.clone(),
                kind: device.kind.clone(),
                name: device.name.clone(),
                vram_gb: vram,
                cuda_index: None,
            };
            let card = card_for_devices(
                vec![pool_dev],
                PoolStrategy::Metal,
                vram.max(1),
                device.name.clone(),
                Vec::new(),
                Vec::new(),
                false,
            );
            slots.push(ComputeSlot {
                id: format!("metal-{i}"),
                kind: "metal".into(),
                priority: 15,
                card,
                cuda_visible: Vec::new(),
                tp_group: None,
            });
        }
    }

    if vulkan_runtime_supported() {
        for (i, device) in vulkan_discrete.iter().enumerate() {
            let vram = effective_vram_gb(device);
            let pool_dev = PoolDevice {
                id: device.id.clone(),
                kind: device.kind.clone(),
                name: device.name.clone(),
                vram_gb: vram,
                cuda_index: None,
            };
            let card = card_for_devices(
                vec![pool_dev],
                PoolStrategy::Vulkan,
                vram.max(1),
                device.name.clone(),
                Vec::new(),
                Vec::new(),
                true,
            );
            slots.push(ComputeSlot {
                id: format!("vulkan-{i}"),
                kind: "discrete_vulkan".into(),
                priority: 20,
                card,
                cuda_visible: Vec::new(),
                tp_group: None,
            });
        }
        for (i, device) in integrated.iter().enumerate() {
            let vram = effective_vram_gb(device);
            let pool_dev = PoolDevice {
                id: device.id.clone(),
                kind: device.kind.clone(),
                name: device.name.clone(),
                vram_gb: vram,
                cuda_index: None,
            };
            let card = card_for_devices(
                vec![pool_dev],
                PoolStrategy::Vulkan,
                vram.max(1),
                device.name.clone(),
                Vec::new(),
                Vec::new(),
                true,
            );
            slots.push(ComputeSlot {
                id: format!("igpu-{i}"),
                kind: "integrated".into(),
                priority: 40,
                card,
                cuda_visible: Vec::new(),
                tp_group: None,
            });
        }
    }

    // Unified memory: cpu-0 also calls LlamaBackend::init(), which brings Metal
    // up in a second process and fights metal-0 for the same RAM. Keep CPU as a
    // slot only when there is no Metal GPU (CPU-only Mac / Metal disabled).
    if !(metal_runtime_supported() && !metal.is_empty()) {
        let cpu_device = cpu.cloned().unwrap_or_else(|| ComputeDevice {
            id: "cpu:0".into(),
            kind: "cpu".into(),
            name: "CPU".into(),
            vram_gb: None,
            vram_used_gb: None,
            util_pct: None,
            enabled: true,
        });
        let cpu_pool = PoolDevice {
            id: cpu_device.id.clone(),
            kind: "cpu".into(),
            name: cpu_device.name.clone(),
            vram_gb: 0,
            cuda_index: None,
        };
        slots.push(ComputeSlot {
            id: "cpu-0".into(),
            kind: "cpu".into(),
            priority: 90,
            card: card_for_devices(
                vec![cpu_pool],
                PoolStrategy::CpuOnly,
                0,
                cpu_device.name,
                Vec::new(),
                Vec::new(),
                false,
            ),
            cuda_visible: Vec::new(),
            tp_group: None,
        });
    }

    if slots.is_empty() {
        bail!("no compute slots built");
    }
    Ok(ComputePlan { slots, tp_groups })
}

/// Build a TensorParallel VirtualCard for a homogeneous CUDA group (worker-local ids 0..n-1).
pub fn build_tp_card_for_group(
    devices: &[ComputeDevice],
    physical_cuda_ids: &[u32],
) -> Result<VirtualCard> {
    let mut pool_devices = Vec::new();
    let mut vrams = Vec::new();
    let mut names = Vec::new();
    for phys in physical_cuda_ids {
        let device = devices
            .iter()
            .find(|d| parse_cuda_index(&d.id) == Some(*phys))
            .ok_or_else(|| anyhow::anyhow!("missing CUDA device {phys}"))?;
        let vram = effective_vram_gb(device);
        vrams.push(vram);
        names.push(device.name.clone());
        pool_devices.push(PoolDevice {
            id: device.id.clone(),
            kind: device.kind.clone(),
            name: device.name.clone(),
            vram_gb: vram,
            // Remapped: worker CVD lists physical ids in order → llama 0..n-1
            cuda_index: Some(pool_devices.len() as u32),
        });
    }
    let total: u32 = vrams.iter().copied().sum();
    let llama_ids: Vec<u32> = (0..physical_cuda_ids.len() as u32).collect();
    Ok(card_for_devices(
        pool_devices,
        PoolStrategy::TensorParallel,
        total,
        format!("Virtual {} ({} GPUs)", format_vram(total), physical_cuda_ids.len()),
        vram_proportions(&vrams),
        llama_ids,
        false,
    ))
}

/// Apply CUDA_VISIBLE_DEVICES / GGML_VK_VISIBLE_DEVICES for a worker before llama init.
///
/// CpuOnly workers must hide **both** CUDA and Vulkan. Hiding only CUDA still lets
/// ggml-vulkan see NVIDIA devices and allocate VRAM — under concurrent CUDA offload
/// that OOMs (`ErrorOutOfDeviceMemory`) and damages the machine.
/// Legacy helper: pin CUDA devices and hide Vulkan (CUDA worker default).
#[allow(dead_code)]
pub fn apply_slot_cuda_visibility(cuda_visible: &[u32]) {
    let strategy = if cuda_visible.is_empty() {
        PoolStrategy::CpuOnly
    } else {
        PoolStrategy::Single
    };
    apply_slot_backend_visibility(strategy, cuda_visible);
}

pub fn apply_slot_backend_visibility(strategy: PoolStrategy, cuda_visible: &[u32]) {
    match strategy {
        PoolStrategy::Vulkan => {
            // Vulkan slot: no CUDA; leave VK devices visible for ggml.
            std::env::set_var("CUDA_VISIBLE_DEVICES", "");
            std::env::remove_var("GGML_VK_VISIBLE_DEVICES");
        }
        PoolStrategy::Metal => {
            std::env::set_var("CUDA_VISIBLE_DEVICES", "");
            std::env::set_var("GGML_VK_VISIBLE_DEVICES", "");
        }
        PoolStrategy::CpuOnly => {
            std::env::set_var("CUDA_VISIBLE_DEVICES", "");
            // Empty = no Vulkan devices (same convention as CUDA_VISIBLE_DEVICES).
            std::env::set_var("GGML_VK_VISIBLE_DEVICES", "");
        }
        PoolStrategy::Single | PoolStrategy::TensorParallel => {
            pin_cuda_indices_to_pci_bus();
            if cuda_visible.is_empty() {
                std::env::set_var("CUDA_VISIBLE_DEVICES", "");
            } else {
                let joined = cuda_visible
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                std::env::set_var("CUDA_VISIBLE_DEVICES", joined);
            }
            // CUDA workers must not also bind Vulkan (dual-backend VRAM fights).
            std::env::set_var("GGML_VK_VISIBLE_DEVICES", "");
        }
    }
}

pub fn build_virtual_card(devices: &[ComputeDevice]) -> Result<VirtualCard> {
    let enabled: Vec<&ComputeDevice> = devices.iter().filter(|d| d.enabled).collect();
    if enabled.is_empty() {
        bail!("no compute devices enabled");
    }

    let mut pool_devices = Vec::new();
    let mut cuda_ids = Vec::new();
    let mut cuda_vram = Vec::new();
    let mut cuda_names = Vec::new();
    let mut vulkan_vram = Vec::new();
    let mut vulkan_names = Vec::new();
    let mut metal_vram = Vec::new();
    let mut metal_names = Vec::new();

    for device in &enabled {
        let vram_gb = effective_vram_gb(device);
        let cuda_index = parse_cuda_index(&device.id);

        if device.kind == "discrete" && cuda_index.is_some() {
            cuda_ids.push(cuda_index.unwrap());
            cuda_vram.push(vram_gb);
            cuda_names.push(device.name.clone());
        } else if is_metal_accelerator(device) {
            metal_vram.push(vram_gb);
            metal_names.push(device.name.clone());
        } else if is_vulkan_accelerator(device) {
            vulkan_vram.push(vram_gb);
            vulkan_names.push(device.name.clone());
        }

        pool_devices.push(PoolDevice {
            id: device.id.clone(),
            kind: device.kind.clone(),
            name: device.name.clone(),
            vram_gb,
            cuda_index,
        });
    }

    // Prefer CUDA when any NVIDIA device is enabled. Otherwise use Vulkan accelerators
    // (AMD discrete / Intel Arc / iGPU). CPU alone → CpuOnly.
    //
    // Matched multi-GPU → TensorParallel topology (per-model TP still gated at load).
    // Mixed SKU/VRAM → demote to Single on the largest card (and ideally CUDA_VISIBLE_DEVICES).
    let (strategy, total_vram_gb, display_name, tensor_split, uses_vulkan, cuda_ids) =
        if !cuda_ids.is_empty() {
            if cuda_ids.len() > 1 && !cuda_name_vram_homogeneous(&cuda_names, &cuda_vram) {
                let primary_pos = pick_primary_cuda_pos(&cuda_ids, &cuda_vram);
                let physical = cuda_ids[primary_pos];
                let llama_id = if cuda_visibility_remapped_to_zero(physical) {
                    0
                } else {
                    physical
                };
                let vram = cuda_vram[primary_pos];
                let name = cuda_names[primary_pos].clone();
                warn!(
                    kept = %name,
                    llama_cuda_id = llama_id,
                    physical_cuda_id = physical,
                    ignored_gpus = cuda_ids.len() - 1,
                    "mixed NVIDIA GPUs: inference pool demoted to single largest card"
                );
                (
                    PoolStrategy::Single,
                    vram,
                    name,
                    Vec::new(),
                    false,
                    vec![llama_id],
                )
            } else {
                let total: u32 = cuda_vram.iter().copied().sum();
                let name = match cuda_names.len() {
                    1 => cuda_names[0].clone(),
                    n => format!("Virtual {} ({} GPUs)", format_vram(total), n),
                };
                let strategy = if cuda_ids.len() > 1 {
                    PoolStrategy::TensorParallel
                } else {
                    PoolStrategy::Single
                };
                let split = if cuda_ids.len() > 1 {
                    vram_proportions(&cuda_vram)
                } else {
                    Vec::new()
                };
                (strategy, total, name, split, false, cuda_ids)
            }
        } else if !metal_names.is_empty() && metal_runtime_supported() {
            let total: u32 = metal_vram.iter().copied().sum::<u32>().max(1);
            let name = metal_names[0].clone();
            (
                PoolStrategy::Metal,
                total,
                name,
                Vec::new(),
                false,
                Vec::new(),
            )
        } else if !vulkan_names.is_empty() && vulkan_runtime_supported() {
            let total: u32 = vulkan_vram.iter().copied().sum::<u32>().max(1);
            let name = match vulkan_names.len() {
                1 => vulkan_names[0].clone(),
                n => format!("Virtual {} ({} GPUs · Vulkan)", format_vram(total), n),
            };
            (
                PoolStrategy::Vulkan,
                total,
                name,
                Vec::new(),
                true,
                Vec::new(),
            )
        } else {
            let name = if pool_devices.len() == 1 {
                pool_devices[0].name.clone()
            } else {
                format!("CPU pool ({} devices)", pool_devices.len())
            };
            (
                PoolStrategy::CpuOnly,
                0,
                name,
                Vec::new(),
                false,
                Vec::new(),
            )
        };

    // Offload budgets are for the primary GPU (largest), even on TP pools — cascade
    // offload tiers always pin a single device.
    let budget_vram = if !cuda_vram.is_empty() {
        cuda_vram.iter().copied().max().unwrap_or(1)
    } else {
        total_vram_gb.max(1)
    };
    let gpu_layer_budget = match strategy {
        PoolStrategy::CpuOnly => 0,
        _ => offload_layer_budget(budget_vram),
    };

    Ok(VirtualCard {
        devices: pool_devices,
        strategy,
        display_name,
        total_vram_gb,
        tensor_split,
        cuda_device_ids: cuda_ids,
        uses_vulkan,
        gpu_layer_budget,
    })
}

/// AMD discrete, Intel Arc, or integrated GPUs — served via Vulkan when CUDA is absent.
pub fn is_vulkan_accelerator(device: &ComputeDevice) -> bool {
    if is_metal_accelerator(device) {
        return false;
    }
    if device.kind == "cpu" {
        return false;
    }
    if parse_cuda_index(&device.id).is_some() {
        return false;
    }
    if device.kind == "integrated" {
        return true;
    }
    // Discrete non-NVIDIA (amd:*, pci amd/intel arc, etc.)
    device.kind == "discrete"
        && (device.id.starts_with("amd:")
            || device.id.starts_with("pci-amd:")
            || device.id.starts_with("pci-intel:")
            || device.name.to_ascii_lowercase().contains("amd")
            || device.name.to_ascii_lowercase().contains("radeon")
            || device.name.to_ascii_lowercase().contains("arc"))
}

fn effective_vram_gb(device: &ComputeDevice) -> u32 {
    if let Some(v) = device.vram_gb.filter(|v| *v > 0) {
        return v;
    }
    match device.kind.as_str() {
        // Soft estimate for shared iGPU memory — cascade + RAM checks are the real gate.
        "integrated" => 2,
        "discrete" | "metal" => 1,
        _ => 0,
    }
}

/// Linux release builds compile Vulkan in; Windows CUDA-only builds fall back to CPU
/// for AMD/Intel until a Windows Vulkan release exists.
pub fn vulkan_runtime_supported() -> bool {
    cfg!(feature = "vulkan")
}

pub fn is_metal_accelerator(device: &ComputeDevice) -> bool {
    device.kind == "metal" || device.id.starts_with("metal:")
}

pub fn metal_runtime_supported() -> bool {
    cfg!(feature = "metal")
}

/// Conservative layer estimate for CPU-offload fallbacks after a full-GPU OOM
/// when GGUF weight/shape is unknown: leftover MiB / 300 MiB per layer.
pub fn offload_layer_budget(total_discrete_vram_gb: u32) -> u32 {
    const MIB_PER_LAYER: f32 = 300.0;
    const KV_HEADROOM_MIB: f32 = 768.0;
    let usable_mib = (total_discrete_vram_gb as f32 * 1024.0 - KV_HEADROOM_MIB).max(0.0);
    if usable_mib < MIB_PER_LAYER {
        return 0;
    }
    (usable_mib / MIB_PER_LAYER).round() as u32
}

/// CUDA index of the largest enabled NVIDIA GPU (offload / Single fallbacks).
pub fn primary_cuda_device(pool: &VirtualCard) -> Option<u32> {
    let mut best: Option<(u32, u32)> = None; // (vram, cuda_index)
    for d in pool.devices.iter().filter(|d| d.kind == "discrete") {
        let Some(idx) = d.cuda_index else {
            continue;
        };
        let vram = d.vram_gb;
        best = Some(match best {
            None => (vram, idx),
            Some((bv, bi)) => {
                if vram > bv || (vram == bv && idx < bi) {
                    (vram, idx)
                } else {
                    (bv, bi)
                }
            }
        });
    }
    // After mixed-GPU demotion the pool's llama device list is authoritative.
    if pool.strategy == PoolStrategy::Single && pool.cuda_device_ids.len() == 1 {
        return pool.cuda_device_ids.first().copied();
    }
    best.map(|(_, idx)| idx)
}

pub fn primary_cuda_vram_gb(pool: &VirtualCard) -> u32 {
    if pool.strategy == PoolStrategy::Single && pool.cuda_device_ids.len() == 1 {
        return pool.total_vram_gb.max(1);
    }
    pool.devices
        .iter()
        .filter(|d| d.kind == "discrete" && d.cuda_index.is_some())
        .map(|d| d.vram_gb)
        .max()
        .unwrap_or(0)
}

/// Per-device VRAM for CUDA ids in the pool (same order as `cuda_device_ids`).
pub fn cuda_device_vram_gb(pool: &VirtualCard) -> Vec<u32> {
    if pool.strategy == PoolStrategy::Single && pool.cuda_device_ids.len() == 1 {
        return vec![pool.total_vram_gb.max(1)];
    }
    pool.cuda_device_ids
        .iter()
        .filter_map(|id| {
            pool.devices
                .iter()
                .find(|d| d.cuda_index == Some(*id))
                .map(|d| d.vram_gb)
        })
        .collect()
}

pub fn format_vram(gb: u32) -> String {
    format!("{gb}GB")
}

fn parse_cuda_index(id: &str) -> Option<u32> {
    let rest = id.strip_prefix("nvidia:")?;
    rest.parse().ok()
}

/// nvidia-smi (and our `nvidia:N` ids) use PCI bus order. CUDA defaults to
/// FASTEST_FIRST, so `CUDA_VISIBLE_DEVICES=1` on a 1660+3080 box selects the
/// 1660. Set this before any CUDA init so worker CVD matches slot ids.
fn pin_cuda_indices_to_pci_bus() {
    std::env::set_var("CUDA_DEVICE_ORDER", "PCI_BUS_ID");
}

fn vram_proportions(vram_gb: &[u32]) -> Vec<f32> {
    let total = vram_gb.iter().copied().sum::<u32>().max(1) as f32;
    vram_gb
        .iter()
        .map(|gb| *gb as f32 / total)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_amd_without_rocm_uses_vulkan() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "pci-amd:0".into(),
                kind: "discrete".into(),
                name: "AMD Radeon RX 6800".into(),
                vram_gb: None,
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
        ])
        .unwrap();

        if vulkan_runtime_supported() {
            assert_eq!(card.strategy, PoolStrategy::Vulkan);
            assert!(card.uses_vulkan);
            assert_eq!(card.total_vram_gb, 1); // soft estimate without VRAM probe
        } else {
            assert_eq!(card.strategy, PoolStrategy::CpuOnly);
        }
    }

    #[test]
    fn mixed_multi_gpu_demotes_to_single_largest() {
        // Chillblast-like: placement must not keep TensorParallel topology.
        let card = build_virtual_card(&[
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
        ])
        .unwrap();

        assert_eq!(card.strategy, PoolStrategy::Single);
        assert_eq!(card.total_vram_gb, 4);
        assert_eq!(card.cuda_device_ids, vec![0]);
        assert_eq!(card.display_name, "GTX 1650 SUPER");
        assert_eq!(card.gpu_layer_budget, offload_layer_budget(4));
    }

    #[test]
    fn roomy_multi_gpu_uses_tensor_parallel() {
        let card = build_virtual_card(&[
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
        ])
        .unwrap();

        assert_eq!(card.strategy, PoolStrategy::TensorParallel);
        assert_eq!(card.total_vram_gb, 48);
        assert_eq!(card.cuda_device_ids, vec![0, 1]);
        assert_eq!(card.display_name, "Virtual 48GB (2 GPUs)");
    }

    #[test]
    fn multi_gpu_is_one_tensor_parallel_pool() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "RTX 3080".into(),
                vram_gb: Some(16),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "nvidia:1".into(),
                kind: "discrete".into(),
                name: "RTX 3080".into(),
                vram_gb: Some(16),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();

        assert_eq!(card.strategy, PoolStrategy::TensorParallel);
        assert_eq!(card.total_vram_gb, 32);
        assert!(!card.uses_vulkan);
        assert_eq!(card.display_name, "Virtual 32GB (2 GPUs)");
    }

    #[test]
    fn amd_only_uses_vulkan_when_feature_enabled() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "amd:0".into(),
                kind: "discrete".into(),
                name: "AMD Radeon RX 6800".into(),
                vram_gb: Some(16),
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
        ])
        .unwrap();

        if vulkan_runtime_supported() {
            assert_eq!(card.strategy, PoolStrategy::Vulkan);
            assert!(card.uses_vulkan);
            assert!(card.cuda_device_ids.is_empty());
            assert_eq!(card.total_vram_gb, 16);
            assert_eq!(card.display_name, "AMD Radeon RX 6800");
            assert!(card.gpu_layer_budget > 0);
        } else {
            // Windows CUDA-only builds: AMD alone → CPU until Vulkan ships there.
            assert_eq!(card.strategy, PoolStrategy::CpuOnly);
            assert!(!card.uses_vulkan);
        }
    }

    #[test]
    fn integrated_only_uses_vulkan_when_feature_enabled() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "pci:0".into(),
                kind: "integrated".into(),
                name: "Intel UHD Graphics".into(),
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();

        if vulkan_runtime_supported() {
            assert_eq!(card.strategy, PoolStrategy::Vulkan);
            assert!(card.uses_vulkan);
            assert_eq!(card.total_vram_gb, 2); // soft estimate
        } else {
            assert_eq!(card.strategy, PoolStrategy::CpuOnly);
        }
    }

    #[test]
    fn nvidia_preferred_over_amd_when_both_enabled() {
        let card = build_virtual_card(&[
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
                id: "amd:0".into(),
                kind: "discrete".into(),
                name: "AMD Radeon".into(),
                vram_gb: Some(8),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();

        assert_eq!(card.strategy, PoolStrategy::Single);
        assert!(!card.uses_vulkan);
        assert_eq!(card.cuda_device_ids, vec![0]);
        assert_eq!(card.display_name, "RTX 5050");
    }

    #[test]
    fn single_gpu_prefers_full_gpu_strategy() {
        let card = build_virtual_card(&[
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
        ])
        .unwrap();

        assert_eq!(card.strategy, PoolStrategy::Single);
        assert_eq!(card.gpu_layer_budget, offload_layer_budget(8));
        assert_eq!(primary_cuda_device(&card), Some(0));
    }

    #[test]
    fn offload_layer_budget_scales_past_eighty_layers() {
        // 192 GB H100-class: 80-layer clamp would leave most of the card idle.
        let n = offload_layer_budget(192);
        assert!(n > 80, "{n}");
        assert_eq!(n, ((192.0f64 * 1024.0 - 768.0) / 300.0).round() as u32);
        assert_eq!(offload_layer_budget(0), 0);
    }

    #[test]
    fn primary_cuda_picks_largest_gpu() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "Small".into(),
                vram_gb: Some(4),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "nvidia:1".into(),
                kind: "discrete".into(),
                name: "Large".into(),
                vram_gb: Some(16),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();
        assert_eq!(primary_cuda_device(&card), Some(1));
    }

    #[test]
    fn mixed_gpus_become_independent_slots() {
        let plan = build_compute_slots(&[
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
        ])
        .unwrap();
        assert!(plan.tp_groups.is_empty());
        let cuda: Vec<_> = plan
            .slots
            .iter()
            .filter(|s| s.kind == "discrete_cuda")
            .collect();
        assert_eq!(cuda.len(), 2);
        assert!(plan.slots.iter().any(|s| s.id == "cpu-0"));
        assert!(cuda.iter().all(|s| s.tp_group.is_none()));
        assert_eq!(cuda[0].card.cuda_device_ids, vec![0]);
    }

    #[test]
    fn matched_gpus_share_tp_group_and_per_gpu_slots() {
        let plan = build_compute_slots(&[
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
        ])
        .unwrap();
        assert!(plan.tp_groups.contains_key("cuda-tp-0"));
        let cuda: Vec<_> = plan
            .slots
            .iter()
            .filter(|s| s.kind == "discrete_cuda")
            .collect();
        assert_eq!(cuda.len(), 2);
        assert!(cuda.iter().all(|s| s.tp_group.as_deref() == Some("cuda-tp-0")));
    }

    #[test]
    fn apple_silicon_uses_metal_when_feature_enabled() {
        let card = build_virtual_card(&[
            ComputeDevice {
                id: "metal:0".into(),
                kind: "metal".into(),
                name: "Apple M4".into(),
                vram_gb: Some(16),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "cpu:0".into(),
                kind: "cpu".into(),
                name: "Apple M4".into(),
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();
        if metal_runtime_supported() {
            assert_eq!(card.strategy, PoolStrategy::Metal);
            assert_eq!(card.total_vram_gb, 16);
            assert!(!card.uses_vulkan);
        } else {
            assert_eq!(card.strategy, PoolStrategy::CpuOnly);
        }
    }

    #[test]
    fn apple_silicon_does_not_spawn_cpu_overflow_beside_metal() {
        let plan = build_compute_slots(&[
            ComputeDevice {
                id: "metal:0".into(),
                kind: "metal".into(),
                name: "Apple M1 Max GPU".into(),
                vram_gb: Some(52),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "cpu:0".into(),
                kind: "cpu".into(),
                name: "Apple M1 Max".into(),
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();
        if metal_runtime_supported() {
            assert!(plan.slots.iter().any(|s| s.id == "metal-0"));
            assert!(
                !plan.slots.iter().any(|s| s.id == "cpu-0"),
                "cpu-0 beside Metal fights unified memory"
            );
        }
    }

    #[test]
    fn cuda_slot_visibility_uses_pci_bus_order() {
        apply_slot_backend_visibility(PoolStrategy::Single, &[1]);
        assert_eq!(std::env::var("CUDA_DEVICE_ORDER").ok().as_deref(), Some("PCI_BUS_ID"));
        assert_eq!(std::env::var("CUDA_VISIBLE_DEVICES").ok().as_deref(), Some("1"));
    }
}
