use super::ipc::{WorkerBootConfig, WorkerRequest, WorkerResponse};
use super::placement::{
    accelerator_live_can_place, pick_placement, pick_resident_placement, placement_miss_detail,
    Placement,
};
use crate::compute_pool::{build_compute_slots, ComputePlan, ComputeSlot, PoolStrategy};
use crate::gpu_occupancy::{
    smallest_advertised_need_gb, slot_live_free_gb, OccupancyWatch, SlotOccupancyView,
};
use crate::protocol::{CatalogModel, ChatMessage, GeneratedImage, InvokeTimings};
use crate::specs::ComputeDevice;
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, Notify};
use tracing::{info, warn};

static REQ_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_req_id() -> String {
    format!("r{}", REQ_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn warm_model_weight_mb(runtime_model: &str) -> u64 {
    let Some(path) = crate::models::resolve_model_gguf(runtime_model) else {
        return u64::MAX / 4;
    };
    std::fs::metadata(path)
        .map(|m| m.len() / (1024 * 1024))
        .unwrap_or(u64::MAX / 4)
}

fn force_kill_pid(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F", "/T"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SlotStatus {
    pub id: String,
    pub kind: String,
    pub strategy: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "vramGb")]
    pub vram_gb: u32,
    pub busy: bool,
    pub healthy: bool,
    #[serde(rename = "loadedModels", skip_serializing_if = "Vec::is_empty")]
    pub loaded_models: Vec<String>,
    #[serde(rename = "deviceIds")]
    pub device_ids: Vec<String>,
    #[serde(rename = "tpGroup", skip_serializing_if = "Option::is_none")]
    pub tp_group: Option<String>,
    /// Foreign / leftover VRAM is using this idle GPU; not a Scalattice job.
    #[serde(rename = "occupiedExternal", skip_serializing_if = "std::ops::Not::not")]
    pub occupied_external: bool,
}

struct SlotWorker {
    spec: ComputeSlot,
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    busy: bool,
    healthy: bool,
    loaded_models: Vec<String>,
}

/// Worker removed from the map for an in-flight invoke (or warm).
struct SlotCheckout {
    job_id: String,
    since: Instant,
    pid: Option<u32>,
}

pub struct Supervisor {
    plan: ComputePlan,
    devices: Vec<ComputeDevice>,
    workers: Mutex<HashMap<String, SlotWorker>>,
    changed: Notify,
    /// In-flight job_id → cancel signal (kill worker when router abandons).
    job_cancels: Mutex<HashMap<String, Arc<Notify>>>,
    /// Slots whose worker is currently owned by an invoke/warm stack frame.
    checkouts: Mutex<HashMap<String, SlotCheckout>>,
    ram_gb: u32,
    /// On tight RAM, two concurrent GGUF mmaps OOM the box and drop the WebSocket.
    mmap_gate: Mutex<()>,
    occupancy: Mutex<OccupancyWatch>,
    /// NVIDIA slots we refused to start because the driver cannot run this CUDA.
    unusable_slots: HashSet<String>,
    /// Slot ids the server says must not run a model. Replaced on each pong.
    server_blocks: Mutex<HashMap<String, HashSet<String>>>,
    /// Slot ids this process has already seen fail. Kept across pongs.
    local_blocks: Mutex<HashMap<String, HashSet<String>>>,
}

/// Give up only when the worker stops sending progress/token lines.
/// Decode reports every token (throttled to 400ms). 30s is a missed-beat kill
/// once tokens are flowing, not a limit on opening a large model file.
const WORKER_DECODE_SILENCE: Duration = Duration::from_secs(30);
const WORKER_LOAD_SILENCE: Duration = Duration::from_secs(30);
/// Cold open of a large GGUF, and CPU-heavy prefill, can sit quiet for more
/// than 30s before the next progress line. 30s killed a 27B load and Coder on 8 GB.
const WORKER_PREFILL_SILENCE: Duration = Duration::from_secs(180);
/// Decode ceiling, measured from the first token. Slow cards with the cache in
/// system RAM were still generating when 12 minutes cut them off.
const WORKER_DECODE_WALL: Duration = Duration::from_secs(20 * 60);
const WORKER_PREFILL_WALL: Duration = Duration::from_secs(45 * 60);
/// Must exceed prefill wall + decode wall or Full Debug reclaim kills mid-decode.
const STUCK_CHECKOUT: Duration = Duration::from_secs(70 * 60);

fn worker_silence_for_phase(phase: &str) -> Duration {
    match phase.to_ascii_lowercase().as_str() {
        "decode" => WORKER_DECODE_SILENCE,
        // "start" is the gap before the first llama progress line (opening the file).
        "prefill" | "context" | "load" | "start" => WORKER_PREFILL_SILENCE,
        _ => WORKER_LOAD_SILENCE,
    }
}

fn fault_text(code: &str, detail: &str) -> String {
    format!("{code} {detail}").to_ascii_lowercase()
}

fn slot_resource_failure(text: &str) -> bool {
    text.contains("invoke_timeout")
        || text.contains("provider_timeout")
        || text.contains("operator_timeout")
        || text.contains("insufficient_vram")
        || text.contains("no_vision_capacity")
        || text.contains("out of memory")
        || text.contains("out_of_memory")
        || text.contains("model_out_of_memory")
        || text.contains("provider_out_of_memory")
        || text.contains("operator_out_of_memory")
        || text.contains("cudamalloc")
        || text.contains("cuda malloc")
        || text.contains("failed to allocate")
        || text.contains("out of device memory")
        || text.contains("mps backend out of memory")
        || text.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == "oom")
}

fn shared_weight_failure(text: &str) -> bool {
    if text.contains("disk_full")
        || text.contains("disk full")
        || text.contains("no space left")
        || text.contains("model_not_installed")
        || text.contains("model not installed")
        || text.contains("weights not found")
        || text.contains("unknown architecture")
        || text.contains("unknown model architecture")
    {
        return true;
    }
    let missing = text.contains("no such file") || text.contains("failed to open");
    let weight = text.contains("gguf") || text.contains("safetensors") || text.contains("weight");
    missing && weight
}

fn load_failure(text: &str) -> bool {
    text.contains("model_load_failed")
        || text.contains("diffusers load failed")
        || text.contains("diffusers snapshot missing")
}

fn family_from_text(text: &str) -> Option<&'static str> {
    if text.contains("ptx")
        || text.contains("failed to initialize cuda")
        || text.contains("cuda driver")
        || text.contains("libcuda")
        || text.contains("libnvidia")
        || text.contains("cublas")
        || text.contains("nvidia driver")
        || text.contains("isn't compatible with the graphics")
    {
        return Some("nvidia");
    }
    if text.contains("rocm")
        || text.contains("amdgpu")
        || text.contains("hsa_")
        || text.contains("hip error")
        || text.contains("amd driver")
    {
        return Some("amd");
    }
    if text.contains("level-zero")
        || text.contains("level zero")
        || text.contains("ze_result")
        || text.contains("oneapi")
        || text.contains("intel arc")
        || text.contains("intel gpu")
    {
        return Some("intel");
    }
    if text.contains("failed to initialize metal")
        || text.contains("metal shader")
        || text.contains("metal library")
        || text.contains("apple gpu")
    {
        return Some("apple");
    }
    None
}

fn generic_driver_failure(text: &str) -> bool {
    text.contains("graphics driver") || text.contains("driver is too old")
}

/// `slot`, `machine`, `nvidia`, `amd`, `intel`, `apple`, or `origin` (same vendor as the card that failed).
fn fault_scope(code: &str, detail: &str) -> &'static str {
    let text = fault_text(code, detail);
    if slot_resource_failure(&text) {
        return "slot";
    }
    if shared_weight_failure(&text) {
        return "machine";
    }
    if let Some(family) = family_from_text(&text) {
        return family;
    }
    if load_failure(&text) {
        return "machine";
    }
    if generic_driver_failure(&text) {
        return "origin";
    }
    "slot"
}

fn slot_family(slot: &ComputeSlot) -> Option<&'static str> {
    if slot.kind == "cpu" || matches!(slot.card.strategy, PoolStrategy::CpuOnly) {
        return None;
    }
    if slot.kind == "metal" || matches!(slot.card.strategy, PoolStrategy::Metal) {
        return Some("apple");
    }
    let mut intel = false;
    let mut amd = false;
    let mut nvidia = false;
    for device in &slot.card.devices {
        let id = device.id.to_ascii_lowercase();
        if id.contains("intel") {
            intel = true;
        } else if id.contains("amd:") || id.contains("pci-amd") || id.contains("amdgpu") {
            amd = true;
        } else if id.contains("nvidia:") {
            nvidia = true;
        }
    }
    if intel {
        return Some("intel");
    }
    if amd {
        return Some("amd");
    }
    if nvidia || slot_requires_nvidia_cuda(slot) {
        return Some("nvidia");
    }
    None
}

fn slots_in_family<'a>(slots: &'a [ComputeSlot], family: &str) -> Vec<String> {
    slots
        .iter()
        .filter(|slot| slot_family(slot) == Some(family))
        .map(|slot| slot.id.clone())
        .collect()
}

fn slot_requires_nvidia_cuda(slot: &ComputeSlot) -> bool {
    matches!(
        slot.card.strategy,
        PoolStrategy::Single | PoolStrategy::TensorParallel
    )
}

fn nvidia_slots_unusable(slots: &[ComputeSlot], unusable: &HashSet<String>) -> bool {
    slots
        .iter()
        .any(|slot| unusable.contains(&slot.id) && slot_requires_nvidia_cuda(slot))
}

fn worker_wall_for_phase(phase: &str) -> Duration {
    match phase.to_ascii_lowercase().as_str() {
        "decode" => WORKER_DECODE_WALL,
        // start / load / prefill / context — until the first token
        _ => WORKER_PREFILL_WALL,
    }
}

impl Supervisor {
    pub async fn start(devices: &[ComputeDevice]) -> Result<Arc<Self>> {
        let plan = build_compute_slots(devices)?;
        info!(
            slots = plan.slots.len(),
            tp_groups = plan.tp_groups.len(),
            "compute supervisor partitioning slots"
        );
        let cuda_version = crate::specs::detect_cuda_version();
        let block_nvidia = crate::specs::nvidia_driver_too_old(cuda_version.as_deref());
        if block_nvidia {
            warn!(
                cuda = cuda_version.as_deref().unwrap_or(""),
                "{}",
                crate::specs::accelerator_incompatible_message()
            );
        }
        let mut workers = HashMap::new();
        let mut unusable_slots = HashSet::new();
        for slot in &plan.slots {
            if block_nvidia && slot_requires_nvidia_cuda(slot) {
                warn!(
                    slot = %slot.id,
                    "graphics slot not started; NVIDIA driver cannot run this agent"
                );
                unusable_slots.insert(slot.id.clone());
                continue;
            }
            match spawn_worker(slot).await {
                Ok(w) => {
                    if w.healthy {
                        info!(slot = %slot.id, kind = %slot.kind, "slot worker ready");
                    } else {
                        if slot_requires_nvidia_cuda(slot) {
                            warn!(
                                slot = %slot.id,
                                "{}",
                                crate::specs::accelerator_incompatible_message()
                            );
                        } else {
                            warn!(slot = %slot.id, "graphics slot not started");
                        }
                        unusable_slots.insert(slot.id.clone());
                    }
                    workers.insert(slot.id.clone(), w);
                }
                Err(err) => {
                    warn!(slot = %slot.id, error = %err, "failed to spawn slot worker");
                }
            }
        }
        if workers.is_empty() && unusable_slots.is_empty() {
            bail!("no compute slot workers started");
        }
        let ram_gb = crate::specs::detect_ram_gb().unwrap_or(16);
        Ok(Arc::new(Self {
            plan,
            devices: devices.to_vec(),
            workers: Mutex::new(workers),
            changed: Notify::new(),
            job_cancels: Mutex::new(HashMap::new()),
            checkouts: Mutex::new(HashMap::new()),
            ram_gb,
            mmap_gate: Mutex::new(()),
            occupancy: Mutex::new(OccupancyWatch::new()),
            unusable_slots,
            server_blocks: Mutex::new(HashMap::new()),
            local_blocks: Mutex::new(HashMap::new()),
        }))
    }

    pub fn plan(&self) -> &ComputePlan {
        &self.plan
    }

    /// `None` is an old server and must not wipe blocks learned earlier.
    pub async fn apply_server_blocks(&self, blocks: Option<Vec<(String, Vec<String>)>>) {
        let Some(blocks) = blocks else {
            return;
        };
        let mut map = HashMap::new();
        for (model, slots) in blocks {
            let key = model.trim().to_ascii_lowercase();
            if key.is_empty() {
                continue;
            }
            let set = map.entry(key).or_insert_with(HashSet::new);
            for slot in slots {
                let id = slot.trim();
                if !id.is_empty() {
                    set.insert(id.to_string());
                }
            }
        }
        *self.server_blocks.lock().await = map.clone();
        // The server list is the durable set. Drop a local block it has cleared.
        *self.local_blocks.lock().await = map;
    }

    async fn blocked_slot_ids(&self, model_id: &str) -> HashSet<String> {
        let key = model_id.trim().to_ascii_lowercase();
        let mut out = HashSet::new();
        if let Some(ids) = self.server_blocks.lock().await.get(&key) {
            out.extend(ids.iter().cloned());
        }
        if let Some(ids) = self.local_blocks.lock().await.get(&key) {
            out.extend(ids.iter().cloned());
        }
        out
    }

    async fn note_slot_failure(&self, model_id: &str, slot_id: &str, code: &str, detail: &str) {
        let scope = fault_scope(code, detail);
        let mut ids = Vec::new();
        match scope {
            "machine" => ids.extend(self.plan.slots.iter().map(|slot| slot.id.clone())),
            "nvidia" | "amd" | "intel" | "apple" => {
                ids.extend(slots_in_family(&self.plan.slots, scope));
                if ids.is_empty() && !slot_id.is_empty() {
                    ids.push(slot_id.to_string());
                }
            }
            "origin" => {
                let family = self
                    .plan
                    .slots
                    .iter()
                    .find(|slot| slot.id == slot_id)
                    .and_then(slot_family);
                if let Some(family) = family {
                    ids.extend(slots_in_family(&self.plan.slots, family));
                }
            }
            _ => {}
        }
        if ids.is_empty() && !slot_id.is_empty() && scope != "machine" {
            let text = fault_text(code, detail);
            if slot_resource_failure(&text) {
                ids.push(slot_id.to_string());
            }
        }
        if ids.is_empty() {
            return;
        }
        let key = model_id.trim().to_ascii_lowercase();
        if key.is_empty() {
            return;
        }
        let mut local = self.local_blocks.lock().await;
        let set = local.entry(key).or_insert_with(HashSet::new);
        for id in ids {
            set.insert(id);
        }
    }

    async fn register_job_cancel(&self, job_id: &str) -> Arc<Notify> {
        let notify = Arc::new(Notify::new());
        self.job_cancels
            .lock()
            .await
            .insert(job_id.to_string(), notify.clone());
        notify
    }

    async fn clear_job_cancel(&self, job_id: &str) {
        self.job_cancels.lock().await.remove(job_id);
    }

    /// Kill an in-flight invoke (router abandon / timeout). Returns true if a job was signaled.
    pub async fn cancel_invoke(&self, job_id: &str) -> bool {
        let Some(notify) = self.job_cancels.lock().await.get(job_id).cloned() else {
            return false;
        };
        notify.notify_waiters();
        true
    }

    /// Signal every registered in-flight job to abort (admin stop-all / session reconnect).
    pub async fn cancel_all_invokes(&self) -> usize {
        let cancels: Vec<Arc<Notify>> = self.job_cancels.lock().await.values().cloned().collect();
        let n = cancels.len();
        for notify in cancels {
            notify.notify_waiters();
        }
        n
    }

    /// Cancel in-flight work and wait for workers to die so the next load does
    /// not mmap beside a still-resident GGUF (16 GB boxes reset the WebSocket).
    pub async fn cancel_all_invokes_and_drain(&self, timeout: Duration) -> usize {
        let n = self.cancel_all_invokes().await;
        let deadline = Instant::now() + timeout;
        while self.has_in_flight_work().await {
            if Instant::now() >= deadline {
                let checkouts = self.checkouts.lock().await;
                for (slot, checkout) in checkouts.iter() {
                    if let Some(pid) = checkout.pid {
                        warn!(slot = %slot, pid, "force-killing leftover checkout after cancel");
                        force_kill_pid(pid);
                    }
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        n
    }

    async fn lock_mmap_if_needed(
        &self,
        incoming_weight_gb: f64,
    ) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        if self.ram_has_room_for_mmap(incoming_weight_gb) {
            None
        } else {
            Some(self.mmap_gate.lock().await)
        }
    }

    /// True when remaining system RAM can hold `incoming_weight_gb` on top of
    /// what is already used, after OS reserve. Not a machine-size class.
    fn ram_has_room_for_mmap(&self, incoming_weight_gb: f64) -> bool {
        if incoming_weight_gb <= 0.0 {
            return true;
        }
        let ram = f64::from(self.ram_gb.max(1));
        let used = f64::from(crate::specs::detect_ram_used_gb().unwrap_or(0));
        let reserve = crate::specs::system_ram_reserve_gb(self.ram_gb);
        let avail = (ram - used).max(0.0);
        avail + 0.05 >= incoming_weight_gb + reserve
    }

    /// True when the supervisor has real in-flight work (checked-out slots or cancel waiters).
    pub async fn has_in_flight_work(&self) -> bool {
        !self.checkouts.lock().await.is_empty() || !self.job_cancels.lock().await.is_empty()
    }

    async fn mark_checkout(&self, slot_id: &str, job_id: &str, pid: Option<u32>) {
        self.checkouts.lock().await.insert(
            slot_id.to_string(),
            SlotCheckout {
                job_id: job_id.to_string(),
                since: Instant::now(),
                pid,
            },
        );
    }

    async fn clear_checkout(&self, slot_id: &str) {
        self.checkouts.lock().await.remove(slot_id);
    }

    /// Put a worker back after invoke/warm. If reconcile already respawned a healthy
    /// idle worker into this slot, kill the returning process instead of clobbering it.
    async fn return_worker(&self, slot_id: String, mut worker: SlotWorker) {
        worker.busy = false;
        self.clear_checkout(&slot_id).await;
        let mut workers = self.workers.lock().await;
        if let Some(existing) = workers.get(&slot_id) {
            if existing.healthy && !existing.busy {
                warn!(
                    slot = %slot_id,
                    "discarding returning worker; slot already reclaimed"
                );
                let _ = worker.child.kill().await;
                let _ = worker.child.wait().await;
                return;
            }
        }
        workers.insert(slot_id, worker);
        drop(workers);
        self.changed.notify_waiters();
    }

    /// Detect lied-about busy: orphan `busy` flags, stale checkouts, missing workers.
    /// Safe to call from the heartbeat path under load.
    pub async fn reconcile_slots(&self) -> u32 {
        let mut recovered = 0u32;

        // Clear busy flags on workers that are still in the map but not checked out
        // (warm/invoke crash left busy=true while the process is idle).
        {
            let checkouts = self.checkouts.lock().await;
            let mut workers = self.workers.lock().await;
            for (id, worker) in workers.iter_mut() {
                if worker.busy && !checkouts.contains_key(id) {
                    warn!(slot = %id, "clearing orphan busy flag (worker idle in map)");
                    worker.busy = false;
                    recovered += 1;
                }
            }
        }

        // Stale checkouts: cancel the job, kill the orphaned PID, respawn into the map.
        let stale: Vec<(String, SlotCheckout)> = {
            let checkouts = self.checkouts.lock().await;
            checkouts
                .iter()
                .filter(|(_, c)| c.since.elapsed() >= STUCK_CHECKOUT)
                .map(|(id, c)| {
                    (
                        id.clone(),
                        SlotCheckout {
                            job_id: c.job_id.clone(),
                            since: c.since,
                            pid: c.pid,
                        },
                    )
                })
                .collect()
        };

        for (slot_id, checkout) in stale {
            warn!(
                slot = %slot_id,
                job_id = %checkout.job_id,
                age_s = checkout.since.elapsed().as_secs(),
                "reclaiming stuck checked-out slot"
            );
            let _ = self.cancel_invoke(&checkout.job_id).await;
            if let Some(pid) = checkout.pid {
                force_kill_pid(pid);
            }
            self.clear_checkout(&slot_id).await;
            self.clear_job_cancel(&checkout.job_id).await;

            let mut workers = self.workers.lock().await;
            if workers
                .get(&slot_id)
                .map(|w| w.healthy && !w.busy)
                .unwrap_or(false)
            {
                continue;
            }
            workers.remove(&slot_id);
            drop(workers);

            if let Some(spec) = self.plan.slots.iter().find(|s| s.id == slot_id) {
                match spawn_worker(spec).await {
                    Ok(mut w) => {
                        w.busy = false;
                        self.workers.lock().await.insert(slot_id.clone(), w);
                        recovered += 1;
                        info!(slot = %slot_id, "respawned worker after stuck checkout");
                    }
                    Err(err) => {
                        warn!(slot = %slot_id, error = %err, "failed to respawn after stuck checkout")
                    }
                }
            }
        }

        // Plan slots that exist in neither map nor checkouts (lost after panic).
        let missing: Vec<ComputeSlot> = {
            let workers = self.workers.lock().await;
            let checkouts = self.checkouts.lock().await;
            self.plan
                .slots
                .iter()
                .filter(|s| !workers.contains_key(&s.id) && !checkouts.contains_key(&s.id))
                .cloned()
                .collect()
        };
        for spec in missing {
            warn!(slot = %spec.id, "slot worker missing; respawning");
            match spawn_worker(&spec).await {
                Ok(mut w) => {
                    w.busy = false;
                    self.workers.lock().await.insert(spec.id.clone(), w);
                    recovered += 1;
                }
                Err(err) => warn!(slot = %spec.id, error = %err, "failed to respawn missing slot"),
            }
        }

        if recovered > 0 {
            self.changed.notify_waiters();
        }
        recovered
    }

    pub async fn slot_statuses(&self) -> Vec<SlotStatus> {
        let occupied = self.occupancy.lock().await.latched_ids();
        let workers = self.workers.lock().await;
        self.plan
            .slots
            .iter()
            .map(|spec| {
                // Missing from the map = temporarily checked out for an in-flight invoke.
                let (busy, healthy, loaded) = if self.unusable_slots.contains(&spec.id) {
                    (false, false, Vec::new())
                } else {
                    workers
                        .get(&spec.id)
                        .map(|w| (w.busy, w.healthy, w.loaded_models.clone()))
                        .unwrap_or((true, true, Vec::new()))
                };
                SlotStatus {
                    id: spec.id.clone(),
                    kind: spec.kind.clone(),
                    strategy: spec.card.strategy.as_str().to_string(),
                    display_name: spec.card.display_name.clone(),
                    vram_gb: spec.card.total_vram_gb,
                    busy,
                    healthy,
                    loaded_models: loaded,
                    device_ids: spec.card.devices.iter().map(|d| d.id.clone()).collect(),
                    tp_group: spec.tp_group.clone(),
                    occupied_external: occupied.contains(&spec.id),
                }
            })
            .collect()
    }

    pub async fn idle_slot_ids(&self) -> Vec<String> {
        let workers = self.workers.lock().await;
        self.plan
            .slots
            .iter()
            .filter(|s| {
                workers
                    .get(&s.id)
                    .map(|w| w.healthy && !w.busy)
                    .unwrap_or(false)
            })
            .map(|s| s.id.clone())
            .collect()
    }

    #[allow(dead_code)]
    pub async fn idle_slot_count(&self) -> u32 {
        self.idle_slot_ids().await.len() as u32
    }

    fn slot_is_cpu(&self, id: &str) -> bool {
        self.plan
            .slots
            .iter()
            .any(|s| s.id == id && s.kind == "cpu")
    }

    pub async fn occupied_slot_ids(&self) -> HashSet<String> {
        self.occupancy.lock().await.latched_ids()
    }

    /// Machine-wide driver fault. A failed Vulkan or Metal slot stays unusable
    /// on its own; it does not mean the NVIDIA driver cannot run jobs.
    pub fn accelerator_incompatible(&self) -> bool {
        nvidia_slots_unusable(&self.plan.slots, &self.unusable_slots)
    }

    /// A graphics slot that started. One stderr warning does not make this false
    /// while that slot is still taking jobs.
    pub fn usable_accelerator_slot(&self) -> bool {
        self.plan.slots.iter().any(|slot| {
            slot.kind != "cpu" && !self.unusable_slots.contains(&slot.id)
        })
    }

    /// Idle slots the router may fill. CPU is hidden while a healthy graphics
    /// worker exists, including when that worker is busy. A graphics slot that
    /// never started (driver too old) is not healthy, so the processor slot
    /// stays available.
    pub async fn routing_idle_slot_ids(&self) -> Vec<String> {
        let occupied = self.occupied_slot_ids().await;
        let idle = self.idle_slot_ids().await;
        let healthy_accel = {
            let workers = self.workers.lock().await;
            self.plan.slots.iter().any(|slot| {
                slot.kind != "cpu" && workers.get(&slot.id).is_some_and(|worker| worker.healthy)
            })
        };
        idle.into_iter()
            .filter(|id| {
                if occupied.contains(id) {
                    return false;
                }
                if healthy_accel && self.slot_is_cpu(id) {
                    return false;
                }
                true
            })
            .collect()
    }

    pub async fn routing_idle_slot_count(&self) -> u32 {
        self.routing_idle_slot_ids().await.len() as u32
    }

    /// Every placeable GPU is taken by leftover / foreign VRAM — not our job.
    pub async fn gpus_occupied(&self) -> bool {
        if self.occupied_slot_ids().await.is_empty() {
            return false;
        }
        self.routing_idle_slot_count().await == 0
    }

    pub async fn note_our_vram_activity(&self) {
        self.occupancy.lock().await.note_our_vram(Instant::now());
    }

    pub async fn refresh_gpu_occupancy(
        &self,
        catalog: &[CatalogModel],
        advertised: &[String],
        ram_gb: u32,
        cpu_ram_headroom_gb: u32,
        live_cuda: HashMap<u32, f64>,
    ) {
        let views: Vec<SlotOccupancyView> = {
            let workers = self.workers.lock().await;
            self.plan
                .slots
                .iter()
                .map(|slot| {
                    let worker = workers.get(&slot.id);
                    SlotOccupancyView {
                        slot_id: slot.id.clone(),
                        kind: slot.kind.clone(),
                        strategy: slot.card.strategy,
                        worker_busy: worker.map(|w| w.busy).unwrap_or(true),
                        loaded_models: worker
                            .map(|w| w.loaded_models.clone())
                            .unwrap_or_default(),
                        live_free_gb: slot_live_free_gb(slot, &live_cuda),
                        min_need_gb: smallest_advertised_need_gb(
                            &slot.card,
                            catalog,
                            advertised,
                            ram_gb,
                            cpu_ram_headroom_gb,
                        ),
                    }
                })
                .collect()
        };
        let mut occ = self.occupancy.lock().await;
        let before = occ.latched_ids();
        let after = occ.update(Instant::now(), &views);
        drop(occ);
        for id in after.difference(&before) {
            info!(slot = %id, "GPU occupied by other software; skipping until enough VRAM is free");
        }
        for id in before.difference(&after) {
            info!(slot = %id, "GPU occupancy cleared; slot is placeable again");
        }
    }

    pub async fn max_concurrent_jobs(&self) -> u32 {
        // Advertise accelerator parallelism only. The CPU slot can run one job
        // when no accelerator can host it, but it is not an extra lane: a
        // second job would load another copy of the weights and OOM the box.
        let workers = self.workers.lock().await;
        let accel = self
            .plan
            .slots
            .iter()
            .filter(|s| s.kind != "cpu")
            .filter(|s| !self.unusable_slots.contains(&s.id))
            .filter(|s| workers.get(&s.id).map(|w| w.healthy).unwrap_or(true))
            .count();
        accel.max(1) as u32
    }

    pub async fn loaded_models_union(&self) -> Vec<String> {
        let workers = self.workers.lock().await;
        let mut set = std::collections::BTreeSet::new();
        for w in workers.values() {
            for m in &w.loaded_models {
                set.insert(m.clone());
            }
        }
        set.into_iter().collect()
    }

    pub async fn evict_all(&self) {
        let mut workers = self.workers.lock().await;
        for (id, worker) in workers.iter_mut() {
            let req_id = next_req_id();
            if let Err(err) = worker_rpc(worker, WorkerRequest::Evict { id: req_id }).await {
                warn!(slot = %id, error = %err, "evict failed");
            }
            worker.loaded_models.clear();
        }
        drop(workers);
        self.note_our_vram_activity().await;
    }

    /// Preload only the runtime the Go hypervisor named. Empty = stay empty.
    pub async fn warm_models(&self, runtime_models: &[String]) -> Result<bool> {
        if runtime_models.is_empty() {
            return Ok(false);
        }
        let idle = self.idle_slot_ids().await;
        let occupied = self.occupied_slot_ids().await;
        let has_accel = self.plan.slots.iter().any(|s| s.kind != "cpu");
        // Never fall back to warming cpu-0 while GPUs exist but are busy.
        let targets: Vec<String> = idle
            .into_iter()
            .filter(|id| !occupied.contains(id))
            .filter(|id| {
                if id.starts_with("cpu-") {
                    !has_accel
                } else {
                    true
                }
            })
            .collect();
        if targets.is_empty() {
            return Ok(false);
        }

        let mut warmed_any = false;
        for slot_id in targets {
            let Some(model) = runtime_models.first().cloned() else {
                continue;
            };

            let incoming_gb = warm_model_weight_mb(&model) as f64 / 1024.0;
            let _mmap = self.lock_mmap_if_needed(incoming_gb).await;

            let mut workers = self.workers.lock().await;
            let Some(worker) = workers.get_mut(&slot_id) else {
                continue;
            };
            if worker.busy || !worker.healthy {
                continue;
            }
            // Already resident: keep it. Advisory warm must not evict/offload a
            // warm model just to chase a different catalog preference.
            if !worker.loaded_models.is_empty() {
                warmed_any = true;
                continue;
            }
            worker.busy = true;
            drop(workers);

            // Take the worker out so invoke can claim other slots while this
            // load runs (holding the map lock during Warm blocked debug for minutes).
            let mut worker = {
                let mut workers = self.workers.lock().await;
                match workers.remove(&slot_id) {
                    Some(w) => w,
                    None => continue,
                }
            };
            let pid = worker.child.id();
            self.mark_checkout(&slot_id, &format!("warm:{slot_id}"), pid)
                .await;
            let req_id = next_req_id();
            let outcome = worker_rpc(
                &mut worker,
                WorkerRequest::Warm {
                    id: req_id,
                    runtime_model: model.clone(),
                },
            )
            .await;
            match &outcome {
                Ok(WorkerResponse::Ok { .. }) => {
                    if !worker.loaded_models.iter().any(|m| m == &model) {
                        worker.loaded_models.push(model.clone());
                    }
                    warmed_any = true;
                    info!(slot = %slot_id, model = %model, "warmed model on slot");
                }
                Ok(WorkerResponse::Error { error, .. }) => {
                    warn!(slot = %slot_id, model = %model, error = %error, "warm failed");
                }
                Ok(_) => {}
                Err(err) => warn!(slot = %slot_id, error = %err, "warm rpc failed"),
            }
            self.return_worker(slot_id, worker).await;
        }
        Ok(warmed_any)
    }

    pub async fn invoke(
        &self,
        job_id: &str,
        model_id: &str,
        runtime_model: &str,
        messages: &[ChatMessage],
        max_tokens: u32,
        model: &CatalogModel,
        ram_gb: u32,
        cpu_ram_headroom_gb: u32,
        on_delta: Option<Box<dyn FnMut(String) + Send>>,
    ) -> Result<(String, u32, u32, InvokeTimings, String)> {
        let cancel = self.register_job_cancel(job_id).await;
        let incoming_gb = warm_model_weight_mb(runtime_model) as f64 / 1024.0;
        let _mmap = if self.ram_has_room_for_mmap(incoming_gb) {
            None
        } else {
            tokio::select! {
                biased;
                _ = cancel.notified() => {
                    self.clear_job_cancel(job_id).await;
                    bail!("request_canceled");
                }
                guard = self.mmap_gate.lock() => Some(guard),
            }
        };
        let sent_token = Arc::new(AtomicBool::new(false));
        let mut on_delta: Option<Box<dyn FnMut(String) + Send>> = match on_delta {
            None => None,
            Some(mut inner) => {
                let flag = Arc::clone(&sent_token);
                Some(Box::new(move |s: String| {
                    if !s.starts_with('\u{1e}') {
                        flag.store(true, Ordering::Relaxed);
                    }
                    inner(s);
                }))
            }
        };

        let mut skip: HashSet<String> = HashSet::new();
        let mut last_crash: Option<anyhow::Error> = None;
        let accel_slots = self
            .plan
            .slots
            .iter()
            .filter(|s| s.kind != "cpu")
            .count()
            .max(1)
            .min(4);

        for attempt in 0..accel_slots {
            let blocked = self.blocked_slot_ids(model_id).await;
            let placement = {
                let occupied = self.occupied_slot_ids().await;
                let inflight_gpu = match self.checkouts.try_lock() {
                    Ok(guard) => guard
                        .keys()
                        .filter(|id| {
                            self.plan.slots.iter().any(|slot| {
                                slot.id == **id
                                    && slot.kind != "cpu"
                                    && !self.unusable_slots.contains(&slot.id)
                            })
                        })
                        .cloned()
                        .collect::<HashSet<_>>(),
                    // Another task holds the checkout map. Assume a GPU job is in flight
                    // rather than loading a second copy onto the processor.
                    Err(_) => self
                        .plan
                        .slots
                        .iter()
                        .filter(|slot| slot.kind != "cpu" && !self.unusable_slots.contains(&slot.id))
                        .map(|slot| slot.id.clone())
                        .collect::<HashSet<_>>(),
                };
                let mut workers = self.workers.lock().await;
                let live_cuda = crate::specs::live_cuda_free_vram_by_index();
                let gpu_usable = |s: &ComputeSlot| {
                    s.kind != "cpu"
                        && !self.unusable_slots.contains(&s.id)
                        && workers.get(&s.id).is_some_and(|w| w.healthy)
                };
                let idle_gpu_can_place = self.plan.slots.iter().any(|s| {
                    gpu_usable(s)
                        && !skip.contains(&s.id)
                        && !occupied.contains(&s.id)
                        && workers.get(&s.id).is_some_and(|w| !w.busy)
                        && accelerator_live_can_place(s, &live_cuda, model)
                        && crate::models::can_host_model(
                            model,
                            &s.card,
                            ram_gb,
                            cpu_ram_headroom_gb,
                        )
                });
                // Only our own in-flight GPU job blocks the processor. VRAM held
                // by other software is not a job we can wait out, and a card the
                // driver cannot run is not a card that could host this model.
                let busy_gpu_could_host = self.plan.slots.iter().any(|s| {
                    inflight_gpu.contains(&s.id)
                        && crate::models::can_host_model(
                            model,
                            &s.card,
                            ram_gb,
                            cpu_ram_headroom_gb,
                        )
                }) || self.plan.slots.iter().any(|s| {
                    gpu_usable(s)
                        && workers.get(&s.id).is_some_and(|w| w.busy)
                        && crate::models::can_host_model(
                            model,
                            &s.card,
                            ram_gb,
                            cpu_ram_headroom_gb,
                        )
                });
                let has_accel = self.plan.slots.iter().any(|s| s.kind != "cpu");
                let cpu_ram_ok = !has_accel
                    || crate::models::cpu_fallback_fits(model, ram_gb, cpu_ram_headroom_gb);
                let idle: Vec<String> = self
                    .plan
                    .slots
                    .iter()
                    .filter(|s| !skip.contains(&s.id))
                    .filter(|s| !blocked.contains(&s.id))
                    .filter(|s| !occupied.contains(&s.id))
                    .filter(|s| {
                        s.kind != "cpu"
                            || (!idle_gpu_can_place && !busy_gpu_could_host && cpu_ram_ok)
                    })
                    .filter(|s| {
                        workers
                            .get(&s.id)
                            .map(|w| w.healthy && !w.busy)
                            .unwrap_or(false)
                    })
                    .map(|s| s.id.clone())
                    .collect();
                let need_vision = crate::protocol::messages_have_images(messages);
                let resident: Vec<String> = idle
                    .iter()
                    .filter(|id| {
                        workers.get(*id).is_some_and(|worker| {
                            worker.loaded_models.iter().any(|loaded| {
                                loaded.eq_ignore_ascii_case(runtime_model)
                                    || loaded.eq_ignore_ascii_case(model_id)
                            })
                        })
                    })
                    .cloned()
                    .collect();
                let placement = match pick_resident_placement(
                    &self.plan,
                    &idle,
                    &resident,
                    model,
                    ram_gb,
                    cpu_ram_headroom_gb,
                    need_vision,
                )
                .or_else(|| {
                    pick_placement(
                        &self.plan,
                        &idle,
                        model,
                        ram_gb,
                        cpu_ram_headroom_gb,
                        &self.devices,
                        need_vision,
                    )
                }) {
                    Some(p) => p,
                    None => {
                        self.clear_job_cancel(job_id).await;
                        if let Some(err) = last_crash {
                            return Err(err);
                        }
                        let detail = placement_miss_detail(&self.plan, &idle, model, need_vision);
                        return Err(detail.into());
                    }
                };

                for sid in &placement.slot_ids {
                    let worker = match workers.get_mut(sid) {
                        Some(w) => w,
                        None => {
                            self.clear_job_cancel(job_id).await;
                            return Err(anyhow!("slot worker {sid} missing"));
                        }
                    };
                    if worker.busy || !worker.healthy {
                        for claimed in &placement.slot_ids {
                            if claimed == sid {
                                break;
                            }
                            if let Some(w) = workers.get_mut(claimed) {
                                w.busy = false;
                            }
                        }
                        self.clear_job_cancel(job_id).await;
                        return Err(crate::invoke_code::coded(
                            crate::invoke_code::InvokeErrorCode::AgentBusy,
                            format!("slot {sid} not available"),
                        ));
                    }
                    worker.busy = true;
                }
                placement
            };
            self.changed.notify_waiters();

            let result = if placement.use_tp_worker {
                self.invoke_tp(
                    &placement,
                    job_id,
                    model_id,
                    runtime_model,
                    messages,
                    max_tokens,
                    model,
                    on_delta.as_mut(),
                    &cancel,
                )
                .await
            } else {
                self.invoke_single(
                    &placement,
                    job_id,
                    model_id,
                    runtime_model,
                    messages,
                    max_tokens,
                    model,
                    on_delta.as_mut(),
                    &cancel,
                )
                .await
            };

            match result {
                Ok(ok) => {
                    self.clear_job_cancel(job_id).await;
                    return Ok(ok);
                }
                Err(err)
                    if worker_crash_retryable(&err)
                        && !sent_token.load(Ordering::Relaxed)
                        && attempt + 1 < accel_slots =>
                {
                    for sid in &placement.slot_ids {
                        skip.insert(sid.clone());
                    }
                    warn!(
                        attempt = attempt + 1,
                        slots = ?placement.slot_ids,
                        error = %err,
                        "slot worker crashed; retrying invoke on another slot"
                    );
                    last_crash = Some(err);
                }
                Err(err) => {
                    self.clear_job_cancel(job_id).await;
                    let slot = placement
                        .slot_ids
                        .first()
                        .cloned()
                        .unwrap_or_default();
                    if !slot.is_empty() {
                        let code = crate::invoke_code::wire_code(&err);
                        self.note_slot_failure(model_id, &slot, code, &format!("{err:#}"))
                            .await;
                        return Err(err.context(format!("slot_id={slot}")));
                    }
                    return Err(err);
                }
            }
        }

        self.clear_job_cancel(job_id).await;
        Err(last_crash.unwrap_or_else(|| anyhow!("agent_busy: no remaining compute slot")))
    }

    pub async fn invoke_image(
        &self,
        job_id: &str,
        model: &CatalogModel,
        job: crate::image::ImageJob,
        ram_gb: u32,
        cpu_ram_headroom_gb: u32,
        mut on_delta: Option<Box<dyn FnMut(String) + Send>>,
    ) -> Result<(Vec<GeneratedImage>, InvokeTimings, String)> {
        // Before claiming a GPU / killing llama.cpp: image jobs cannot run on a full disk.
        crate::image::refuse_if_disk_full()?;
        let cancel = self.register_job_cancel(job_id).await;
        let started = Instant::now();
        let blocked = self.blocked_slot_ids(&model.model_id).await;
        let placement = {
            let occupied = self.occupied_slot_ids().await;
            let mut workers = self.workers.lock().await;
            let idle: Vec<String> = self
                .plan
                .slots
                .iter()
                .filter(|s| !blocked.contains(&s.id))
                .filter(|s| !occupied.contains(&s.id))
                .filter(|s| {
                    workers
                        .get(&s.id)
                        .map(|w| w.healthy && !w.busy)
                        .unwrap_or(false)
                })
                .map(|s| s.id.clone())
                .collect();
            let placement = match pick_placement(
                &self.plan,
                &idle,
                model,
                ram_gb,
                cpu_ram_headroom_gb,
                &self.devices,
                false,
            ) {
                Some(p) => p,
                None => {
                    self.clear_job_cancel(job_id).await;
                    let detail = placement_miss_detail(&self.plan, &idle, model, false);
                    return Err(detail.into());
                }
            };
            for sid in &placement.slot_ids {
                let Some(worker) = workers.get_mut(sid) else {
                    self.clear_job_cancel(job_id).await;
                    return Err(anyhow!("slot worker {sid} missing"));
                };
                if worker.busy || !worker.healthy {
                    self.clear_job_cancel(job_id).await;
                    return Err(crate::invoke_code::coded(
                        crate::invoke_code::InvokeErrorCode::AgentBusy,
                        format!("slot {sid} not available"),
                    ));
                }
                worker.busy = true;
            }
            placement
        };
        self.changed.notify_waiters();

        let slot_id = placement
            .slot_ids
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("empty placement"))?;
        let image_device = crate::image::image_backend_for_card(&placement.card);
        info!(slot = %slot_id, device = %image_device, job_id, "claimed slot for image job");

        let mut worker = {
            let mut workers = self.workers.lock().await;
            workers
                .remove(&slot_id)
                .ok_or_else(|| anyhow!("slot worker {slot_id} missing"))?
        };
        let pid = worker.child.id();
        self.mark_checkout(&slot_id, job_id, pid).await;

        // Evict llama.cpp so PyTorch can take the same GPU.
        let _ = worker.child.kill().await;
        let _ = worker.child.wait().await;
        // Metal residency sets can linger after SIGKILL; give the OS a beat
        // before the Diffusers process maps the same unified memory.
        let drain = if cfg!(target_os = "macos") {
            Duration::from_millis(1500)
        } else {
            Duration::from_millis(400)
        };
        tokio::time::sleep(drain).await;

        let mut on_progress = |phase: &str, pct: Option<f32>| {
            if let Some(cb) = on_delta.as_mut() {
                cb(format!("\u{1e}{}\u{1e}{}", phase, pct.unwrap_or(-1.0)));
            }
        };

        let outcome = tokio::select! {
            biased;
            _ = cancel.notified() => {
                Err(anyhow!("request_canceled"))
            }
            result = crate::image::run_qwen_image(
                &job,
                &placement.cuda_visible,
                image_device,
                &cancel,
                &mut on_progress,
            ) => result,
        };

        match spawn_worker(&worker.spec).await {
            Ok(new_w) => worker = new_w,
            Err(err) => {
                warn!(slot = %slot_id, error = %err, "failed to respawn llama worker after image job");
                worker.healthy = false;
            }
        }
        self.return_worker(slot_id.clone(), worker).await;
        self.clear_job_cancel(job_id).await;

        let images = match outcome {
            Ok(images) => images,
            Err(err) => {
                let code = crate::invoke_code::wire_code(&err);
                self.note_slot_failure(&model.model_id, &slot_id, code, &format!("{err:#}"))
                    .await;
                return Err(err.context(format!("slot_id={slot_id}")));
            }
        };
        let timings = InvokeTimings {
            model_load_ms: None,
            prefill_ms: None,
            decode_ms: None,
            total_ms: Some(started.elapsed().as_millis() as u64),
        };
        Ok((images, timings, slot_id))
    }

    async fn invoke_single(
        &self,
        placement: &Placement,
        job_id: &str,
        model_id: &str,
        runtime_model: &str,
        messages: &[ChatMessage],
        max_tokens: u32,
        model: &CatalogModel,
        on_delta: Option<&mut Box<dyn FnMut(String) + Send>>,
        cancel: &Notify,
    ) -> Result<(String, u32, u32, InvokeTimings, String)> {
        let slot_id = placement
            .slot_ids
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("empty placement"))?;
        info!(
            slot = %slot_id,
            job_id,
            model = %model_id,
            "claimed compute slot"
        );
        // Take worker out of the map so other slots can run in parallel
        // (busy flag was already set under the claim lock in invoke()).
        let mut worker = {
            let mut workers = self.workers.lock().await;
            workers
                .remove(&slot_id)
                .ok_or_else(|| anyhow!("slot worker {slot_id} missing"))?
        };
        let pid = worker.child.id();
        self.mark_checkout(&slot_id, job_id, pid).await;

        let req_id = next_req_id();
        let stream = on_delta.is_some();
        let need_vision = crate::protocol::messages_have_images(messages);
        let (n_ctx, offload_kqv) =
            crate::models::llama_context_plan(model, &placement.card, need_vision);
        if !offload_kqv
            && !matches!(
                placement.card.strategy,
                crate::compute_pool::PoolStrategy::CpuOnly
            )
        {
            let used = crate::specs::detect_ram_used_gb().unwrap_or(0);
            let free = self.ram_gb.saturating_sub(used);
            let need = crate::models::kv_offload_ram_gb(model, need_vision);
            if free < need {
                self.return_worker(slot_id.clone(), worker).await;
                self.clear_checkout(&slot_id).await;
                return Err(crate::invoke_code::coded(
                    crate::invoke_code::InvokeErrorCode::InsufficientVram,
                    format!(
                        "not enough free system RAM to hold the context (need {need} GB, {free} GB free)"
                    ),
                ));
            }
        }
        let outcome = worker_rpc_invoke_cancellable(
            &mut worker,
            WorkerRequest::Invoke {
                id: req_id,
                job_id: job_id.to_string(),
                model_id: model_id.to_string(),
                runtime_model: runtime_model.to_string(),
                messages: messages.to_vec(),
                max_tokens,
                stream,
                n_ctx,
                offload_kqv: Some(offload_kqv),
                chat_template: model.chat_template.clone(),
                thinking: model.thinking.clone(),
            },
            on_delta,
            cancel,
        )
        .await;

        if let Ok((_, _, _, _, loaded)) = &outcome {
            worker.loaded_models = loaded.clone();
        }
        if outcome.is_err() || worker.child.try_wait().ok().flatten().is_some() {
            if outcome.is_err() {
                warn!(slot = %slot_id, "slot worker invoke ended with error; respawning");
            } else {
                warn!(slot = %slot_id, "slot worker exited; respawning");
            }
            worker.healthy = false;
            if let Ok(new_w) = spawn_worker(&worker.spec).await {
                worker = new_w;
            }
        }

        self.return_worker(slot_id.clone(), worker).await;
        outcome.map(|(c, p, t, timings, _)| (c, p, t, timings, slot_id))
    }

    async fn invoke_tp(
        &self,
        placement: &Placement,
        job_id: &str,
        model_id: &str,
        runtime_model: &str,
        messages: &[ChatMessage],
        max_tokens: u32,
        model: &CatalogModel,
        on_delta: Option<&mut Box<dyn FnMut(String) + Send>>,
        cancel: &Notify,
    ) -> Result<(String, u32, u32, InvokeTimings, String)> {
        // Slots already claimed busy by invoke(). Pause siblings, run TP, restore.
        let sibling_pids: Vec<(String, Option<u32>)> = {
            let mut workers = self.workers.lock().await;
            let mut out = Vec::new();
            for sid in &placement.slot_ids {
                let Some(w) = workers.get_mut(sid) else {
                    bail!("missing sibling slot {sid}");
                };
                let pid = w.child.id();
                // Stop the process so CVD can be reused by TP worker.
                let _ = w.child.kill().await;
                w.healthy = false;
                out.push((sid.clone(), pid));
            }
            out
        };
        for (sid, pid) in &sibling_pids {
            self.mark_checkout(sid, job_id, *pid).await;
        }

        let tp_key = placement.slot_ids.join("+");
        let tp_spec = ComputeSlot {
            id: format!("tp:{tp_key}"),
            kind: "tensor_parallel".into(),
            priority: 5,
            card: placement.card.clone(),
            cuda_visible: placement.cuda_visible.clone(),
            tp_group: None,
        };

        let mut tp_worker = match spawn_worker(&tp_spec).await {
            Ok(w) => w,
            Err(err) => {
                // Restore siblings so slots aren't stuck busy after a failed claim.
                let mut workers = self.workers.lock().await;
                for sid in &placement.slot_ids {
                    if let Some(spec) = self.plan.slots.iter().find(|s| s.id == *sid) {
                        match spawn_worker(spec).await {
                            Ok(mut w) => {
                                w.busy = false;
                                workers.insert(sid.clone(), w);
                            }
                            Err(restore_err) => {
                                warn!(
                                    slot = %sid,
                                    error = %restore_err,
                                    "failed to restore slot worker after TP spawn failure"
                                );
                                if let Some(w) = workers.get_mut(sid) {
                                    w.busy = false;
                                }
                            }
                        }
                    }
                }
                drop(workers);
                for sid in &placement.slot_ids {
                    self.clear_checkout(sid).await;
                }
                self.changed.notify_waiters();
                return Err(err).context("spawn tensor-parallel worker");
            }
        };
        tp_worker.busy = true;

        let req_id = next_req_id();
        let stream = on_delta.is_some();
        let need_vision = crate::protocol::messages_have_images(messages);
        let (n_ctx, offload_kqv) =
            crate::models::llama_context_plan(model, &placement.card, need_vision);
        let outcome = worker_rpc_invoke_cancellable(
            &mut tp_worker,
            WorkerRequest::Invoke {
                id: req_id,
                job_id: job_id.to_string(),
                model_id: model_id.to_string(),
                runtime_model: runtime_model.to_string(),
                messages: messages.to_vec(),
                max_tokens,
                stream,
                n_ctx,
                offload_kqv: Some(offload_kqv),
                chat_template: model.chat_template.clone(),
                thinking: model.thinking.clone(),
            },
            on_delta,
            cancel,
        )
        .await;

        let _ = tp_worker.child.kill().await;

        // Restore per-GPU workers.
        let mut workers = self.workers.lock().await;
        for sid in &placement.slot_ids {
            if let Some(spec) = self.plan.slots.iter().find(|s| s.id == *sid) {
                match spawn_worker(spec).await {
                    Ok(mut w) => {
                        w.busy = false;
                        workers.insert(sid.clone(), w);
                    }
                    Err(err) => warn!(slot = %sid, error = %err, "failed to restore slot worker"),
                }
            }
        }
        drop(workers);
        for sid in &placement.slot_ids {
            self.clear_checkout(sid).await;
        }
        self.changed.notify_waiters();

        let tp_label = format!("tp:{}", placement.slot_ids.join("+"));
        outcome.map(|(c, p, t, timings, _)| (c, p, t, timings, tp_label))
    }
}

async fn spawn_worker(slot: &ComputeSlot) -> Result<SlotWorker> {
    let boot = WorkerBootConfig {
        slot_id: slot.id.clone(),
        card: slot.card.clone(),
        cuda_visible: slot.cuda_visible.clone(),
    };
    let boot_json = serde_json::to_string(&boot)?;
    // Always re-exec this binary so PATH can't pick an older agent without `worker`.
    let bin = std::env::current_exe().unwrap_or_else(|_| {
        crate::paths::resolve_agent_binary()
            .unwrap_or_else(|_| std::path::PathBuf::from("scalattice-agent"))
    });

    let mut child = Command::new(&bin)
        .arg("worker")
        .env("SCALATTICE_WORKER_CONFIG", &boot_json)
        // Worker logging owns its EnvFilter (full llama detail → agent.log).
        // Do not inherit a supervisor `RUST_LOG=warn` that would drop INFO thoughts.
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Never inherit stderr into a GUI parent: Windows tray apps often don't
        // drain it, so llama/tracing fills the pipe and the worker stalls for minutes.
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn worker for slot {} ({})", slot.id, bin.display()))?;

    let slot_log_id = slot.id.clone();
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let t = line.trim();
                        if !t.is_empty() {
                            // Slot workers own llama.cpp. debug! was dropped by the INFO
                            // filter, so Verbose live logs never saw load dumps.
                            // Strip the worker's own tracing prefix so we don't nest
                            // timestamps/targets when the supervisor re-logs.
                            let (_lvl, body) = crate::cloud_log::normalize_tracing_message(t);
                            if !body.is_empty() {
                                info!(slot = %slot_log_id, "{body}");
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    let stdin = child.stdin.take().context("worker stdin")?;
    let stdout = child.stdout.take().context("worker stdout")?;
    let mut worker = SlotWorker {
        spec: slot.clone(),
        child,
        stdin,
        reader: BufReader::new(stdout),
        busy: false,
        healthy: true,
        loaded_models: Vec::new(),
    };

    // Handshake
    let req_id = next_req_id();
    match worker_rpc(&mut worker, WorkerRequest::Ping { id: req_id }).await {
        Ok(WorkerResponse::Pong { incompatible, .. }) => {
            if incompatible {
                worker.healthy = false;
            }
            Ok(worker)
        }
        Ok(other) => {
            let _ = worker.child.kill().await;
            bail!("unexpected ping response: {other:?}");
        }
        Err(err) => {
            let _ = worker.child.kill().await;
            Err(err)
        }
    }
}

fn try_parse_worker_response(line: &str) -> Option<WorkerResponse> {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.starts_with('{') {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

async fn worker_rpc(worker: &mut SlotWorker, req: WorkerRequest) -> Result<WorkerResponse> {
    let expect_id = request_id(&req);
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    worker.stdin.write_all(line.as_bytes()).await?;
    worker.stdin.flush().await?;

    let mut buf = String::new();
    let mut skipped = 0u32;
    loop {
        buf.clear();
        let n = worker
            .reader
            .read_line(&mut buf)
            .await
            .context("read worker response")?;
        if n == 0 {
            worker.healthy = false;
            return Err(crate::invoke_code::coded(
                crate::invoke_code::InvokeErrorCode::WorkerLost,
                format!("worker closed stdout (after {skipped} non-json line(s))"),
            ));
        }
        let Some(resp) = try_parse_worker_response(&buf) else {
            skipped += 1;
            if skipped <= 8 {
                warn!(
                    slot = %worker.spec.id,
                    line = %buf.trim(),
                    "ignoring non-json worker stdout"
                );
            }
            continue;
        };
        match &resp {
            WorkerResponse::Delta { .. } | WorkerResponse::Progress { .. } => continue,
            WorkerResponse::Pong { id, .. }
            | WorkerResponse::Ok { id }
            | WorkerResponse::Result { id, .. }
            | WorkerResponse::Health { id, .. }
            | WorkerResponse::Error { id, .. } => {
                if id == &expect_id || expect_id == "unknown" {
                    return Ok(resp);
                }
            }
        }
    }
}

async fn worker_rpc_invoke_cancellable(
    worker: &mut SlotWorker,
    req: WorkerRequest,
    mut on_delta: Option<&mut Box<dyn FnMut(String) + Send>>,
    cancel: &Notify,
) -> Result<(String, u32, u32, InvokeTimings, Vec<String>)> {
    let expect_id = request_id(&req);
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    worker.stdin.write_all(line.as_bytes()).await?;
    worker.stdin.flush().await?;

    let mut buf = String::new();
    let mut last_phase = String::from("start");
    let mut silence = worker_silence_for_phase(&last_phase);
    let mut phase_started = Instant::now();
    let mut last_progress = Instant::now();
    loop {
        if phase_started.elapsed() >= worker_wall_for_phase(&last_phase) {
            warn!(
                slot = %worker.spec.id,
                phase = %last_phase,
                wall_s = phase_started.elapsed().as_secs(),
                "killing worker; invoke exceeded wall-clock limit"
            );
            let _ = worker.child.kill().await;
            let _ = worker.child.wait().await;
            worker.healthy = false;
            return Err(crate::invoke_code::coded(
                crate::invoke_code::InvokeErrorCode::InvokeTimeout,
                "exceeded wall-clock limit",
            ));
        }
        buf.clear();
        let silence_left = silence
            .checked_sub(last_progress.elapsed())
            .unwrap_or(Duration::ZERO);
        let wall_left = worker_wall_for_phase(&last_phase)
            .checked_sub(phase_started.elapsed())
            .unwrap_or(Duration::from_millis(1));
        let wait = silence_left.min(wall_left).max(Duration::from_millis(50));
        tokio::select! {
            biased;
            _ = cancel.notified() => {
                info!(slot = %worker.spec.id, "killing worker for canceled invoke");
                let _ = worker.child.kill().await;
                let _ = worker.child.wait().await;
                worker.healthy = false;
                bail!("request_canceled");
            }
            n = worker.reader.read_line(&mut buf) => {
                let n = n.context("read worker invoke response")?;
                if n == 0 {
                    worker.healthy = false;
                    return Err(crate::invoke_code::coded(
                        crate::invoke_code::InvokeErrorCode::WorkerLost,
                        "worker closed stdout during invoke",
                    ));
                }
                let Some(resp) = try_parse_worker_response(&buf) else {
                    warn!(
                        slot = %worker.spec.id,
                        line = %buf.trim(),
                        "ignoring non-json worker stdout during invoke"
                    );
                    continue;
                };
                match resp {
                    WorkerResponse::Progress { id, phase, pct } if id == expect_id => {
                        if last_phase != phase {
                            last_phase = phase.clone();
                            phase_started = Instant::now();
                        }
                        silence = worker_silence_for_phase(&phase);
                        last_progress = Instant::now();
                        if let Some(cb) = on_delta.as_mut() {
                            cb(format!(
                                "\u{1e}{}\u{1e}{}",
                                phase,
                                pct.unwrap_or(-1.0)
                            ));
                        }
                    }
                    WorkerResponse::Delta { id, text } if id == expect_id => {
                        if last_phase != "decode" {
                            last_phase = "decode".to_string();
                            phase_started = Instant::now();
                        }
                        silence = WORKER_DECODE_SILENCE;
                        last_progress = Instant::now();
                        if let Some(cb) = on_delta.as_mut() {
                            cb(text);
                        }
                    }
                    WorkerResponse::Result {
                        id,
                        content,
                        prompt_tokens,
                        completion_tokens,
                        timings,
                        loaded_models,
                    } if id == expect_id => {
                        return Ok((
                            content,
                            prompt_tokens,
                            completion_tokens,
                            timings,
                            loaded_models,
                        ));
                    }
                    WorkerResponse::Error { id, error } if id == expect_id => {
                        return Err(crate::invoke_code::error_from_wire(&error));
                    }
                    other => {
                        warn!(?other, "ignoring unexpected worker message during invoke");
                    }
                }
            }
            _ = tokio::time::sleep(wait) => {
                if last_progress.elapsed() < silence {
                    continue;
                }
                warn!(
                    slot = %worker.spec.id,
                    phase = %last_phase,
                    silence_s = silence.as_secs(),
                    "killing worker; progress comms went silent"
                );
                let _ = worker.child.kill().await;
                let _ = worker.child.wait().await;
                worker.healthy = false;
                return Err(crate::invoke_code::coded(
                    crate::invoke_code::InvokeErrorCode::InvokeTimeout,
                    "worker made no progress",
                ));
            }
        }
    }
}

fn request_id(req: &WorkerRequest) -> String {
    match req {
        WorkerRequest::Ping { id }
        | WorkerRequest::Warm { id, .. }
        | WorkerRequest::Invoke { id, .. }
        | WorkerRequest::Evict { id }
        | WorkerRequest::Health { id }
        | WorkerRequest::Shutdown { id } => id.clone(),
    }
}

/// Worker process died (CUDA abort / stdout close). Retry on another slot
/// unless the client already received tokens or the error is a real reject.
fn worker_crash_retryable(err: &anyhow::Error) -> bool {
    if err
        .chain()
        .any(|cause| cause.downcast_ref::<crate::invoke_code::CodedError>().is_some())
    {
        return crate::invoke_code::crash_retryable(err);
    }
    // A worker that still sends a bare sentence (no code). Our own failures are coded above.
    let d = format!("{err:#}").to_lowercase();
    let benign = matches!(
        crate::invoke_code::code_of(err),
        crate::invoke_code::InvokeErrorCode::RequestCanceled
            | crate::invoke_code::InvokeErrorCode::PromptTooLong
            | crate::invoke_code::InvokeErrorCode::InvalidImage
            | crate::invoke_code::InvokeErrorCode::AgentBusy
            | crate::invoke_code::InvokeErrorCode::NoIdleSlot
            | crate::invoke_code::InvokeErrorCode::InsufficientVram
    );
    if benign && !d.contains("null result") && !d.contains("closed stdout") {
        return false;
    }
    d.contains("closed stdout")
        || d.contains("null result")
        || d.contains("create llama context")
        || d.contains("out of memory")
        || d.contains("cudamalloc")
        || d.contains("cuda error")
}

#[cfg(test)]
mod tests {
    use super::{
        nvidia_slots_unusable, worker_crash_retryable, worker_silence_for_phase,
        worker_wall_for_phase, STUCK_CHECKOUT, WORKER_DECODE_SILENCE, WORKER_DECODE_WALL,
        WORKER_PREFILL_SILENCE, WORKER_PREFILL_WALL,
    };
    use crate::compute_pool::build_compute_slots;
    use crate::specs::ComputeDevice;
    use std::collections::HashSet;

    #[test]
    fn amd_slot_failure_is_not_a_nvidia_driver_fault() {
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
        let mut unusable = HashSet::new();
        unusable.insert("cpu-0".to_string());
        assert!(!nvidia_slots_unusable(&plan.slots, &unusable));
        unusable.insert("cuda-0".to_string());
        assert!(nvidia_slots_unusable(&plan.slots, &unusable));
    }

    #[test]
    fn stdout_close_retries_on_another_slot() {
        let err = anyhow::anyhow!("worker closed stdout during invoke");
        assert!(worker_crash_retryable(&err));
    }

    #[test]
    fn cancel_and_busy_do_not_retry() {
        assert!(!worker_crash_retryable(&anyhow::anyhow!(
            "request_canceled"
        )));
        assert!(!worker_crash_retryable(&anyhow::anyhow!(
            "agent_busy: no idle compute slot"
        )));
    }

    #[test]
    fn prefill_wall_is_longer_than_decode_and_covers_start() {
        assert_eq!(worker_wall_for_phase("decode"), WORKER_DECODE_WALL);
        assert_eq!(worker_wall_for_phase("prefill"), WORKER_PREFILL_WALL);
        assert_eq!(worker_wall_for_phase("start"), WORKER_PREFILL_WALL);
        assert!(WORKER_PREFILL_WALL > WORKER_DECODE_WALL);
        assert!(STUCK_CHECKOUT > WORKER_PREFILL_WALL + WORKER_DECODE_WALL);
    }

    #[test]
    fn cold_load_is_not_killed_at_the_decode_missed_beat() {
        assert_eq!(worker_silence_for_phase("start"), WORKER_PREFILL_SILENCE);
        assert_eq!(worker_silence_for_phase("load"), WORKER_PREFILL_SILENCE);
        assert_eq!(worker_silence_for_phase("decode"), WORKER_DECODE_SILENCE);
    }
}
