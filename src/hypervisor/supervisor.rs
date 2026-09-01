use super::demand::DemandTracker;
use super::ipc::{WorkerBootConfig, WorkerRequest, WorkerResponse};
use super::placement::{pick_placement, placement_miss_detail, Placement};
use crate::compute_pool::{build_compute_slots, ComputePlan, ComputeSlot};
use crate::protocol::{CatalogModel, ChatMessage, InvokeTimings};
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

pub struct Hypervisor {
    plan: ComputePlan,
    devices: Vec<ComputeDevice>,
    workers: Mutex<HashMap<String, SlotWorker>>,
    demand: Mutex<DemandTracker>,
    changed: Notify,
    /// In-flight job_id → cancel signal (kill worker when router abandons).
    job_cancels: Mutex<HashMap<String, Arc<Notify>>>,
    /// Slots whose worker is currently owned by an invoke/warm stack frame.
    checkouts: Mutex<HashMap<String, SlotCheckout>>,
    ram_gb: u32,
    /// On tight RAM, two concurrent GGUF mmaps OOM the box and drop the WebSocket.
    mmap_gate: Mutex<()>,
}

/// Give up only when the worker stops sending progress/token lines.
/// Load / evict / context / prefill can sit in CUDA with no llama.cpp callback
/// (graph compile, weight upload). Only token decode uses the short stall.
const WORKER_DECODE_SILENCE: Duration = Duration::from_secs(120);
const WORKER_LOAD_SILENCE: Duration = Duration::from_secs(300);
/// Hard ceiling for any single invoke, even if the worker keeps dripping tokens.
/// Prevents abandoned streams from holding a GPU forever under network load.
const WORKER_INVOKE_WALL_CLOCK: Duration = Duration::from_secs(12 * 60);
/// Checked-out slot with no progress path for this long → force reclaim.
const STUCK_CHECKOUT: Duration = Duration::from_secs(13 * 60);
/// Hosts at or below this RAM cannot mmap two 5 GB GGUFs at once.
const TIGHT_RAM_GB: u32 = 24;

fn worker_silence_for_phase(phase: &str) -> Duration {
    if phase.eq_ignore_ascii_case("decode") {
        WORKER_DECODE_SILENCE
    } else {
        WORKER_LOAD_SILENCE
    }
}

impl Hypervisor {
    pub async fn start(devices: &[ComputeDevice]) -> Result<Arc<Self>> {
        let plan = build_compute_slots(devices)?;
        info!(
            slots = plan.slots.len(),
            tp_groups = plan.tp_groups.len(),
            "compute hypervisor partitioning slots"
        );
        let mut workers = HashMap::new();
        for slot in &plan.slots {
            match spawn_worker(slot).await {
                Ok(w) => {
                    info!(slot = %slot.id, kind = %slot.kind, "slot worker ready");
                    workers.insert(slot.id.clone(), w);
                }
                Err(err) => {
                    warn!(slot = %slot.id, error = %err, "failed to spawn slot worker");
                }
            }
        }
        if workers.is_empty() {
            bail!("no compute slot workers started");
        }
        let ram_gb = crate::specs::detect_ram_gb().unwrap_or(16);
        Ok(Arc::new(Self {
            plan,
            devices: devices.to_vec(),
            workers: Mutex::new(workers),
            demand: Mutex::new(DemandTracker::default()),
            changed: Notify::new(),
            job_cancels: Mutex::new(HashMap::new()),
            checkouts: Mutex::new(HashMap::new()),
            ram_gb,
            mmap_gate: Mutex::new(()),
        }))
    }

    pub fn plan(&self) -> &ComputePlan {
        &self.plan
    }

    pub async fn record_demand(&self, runtime_model: &str) {
        self.demand.lock().await.record_hit(runtime_model);
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

    async fn lock_mmap_if_tight(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        if self.ram_gb > TIGHT_RAM_GB {
            None
        } else {
            Some(self.mmap_gate.lock().await)
        }
    }

    /// How many slots are currently checked out to an invoke/warm stack.
    #[allow(dead_code)]
    pub async fn checked_out_count(&self) -> u32 {
        self.checkouts.lock().await.len() as u32
    }

    /// True when the hypervisor has real in-flight work (checked-out slots or cancel waiters).
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
            if workers.get(&slot_id).map(|w| w.healthy && !w.busy).unwrap_or(false) {
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

    #[allow(dead_code)]
    pub async fn order_models_by_demand(&self, models: &[String]) -> Vec<String> {
        self.demand
            .lock()
            .await
            .order_by_demand(models, Duration::from_secs(30 * 60))
    }

    /// Demand first; ties prefer models that fit in `max_vram_gb`, then lightest on disk.
    /// Avoids cold-start alphabetical picks like `Qwen/…-7B` over `qwen/qwen3-1.7b`.
    pub async fn order_models_for_warm(&self, models: &[String], max_vram_gb: u32) -> Vec<String> {
        let window = Duration::from_secs(30 * 60);
        let demand = self.demand.lock().await;
        let mut scored: Vec<(u32, u8, u64, String)> = models
            .iter()
            .map(|m| {
                let hits = demand.score(m, window);
                let weight_mb = warm_model_weight_mb(m);
                let fits = max_vram_gb > 0
                    && weight_mb > 0
                    && weight_mb <= u64::from(max_vram_gb).saturating_mul(1024);
                let fit_penalty: u8 = if fits { 0 } else { 1 };
                (hits, fit_penalty, weight_mb, m.clone())
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.3.to_ascii_lowercase().cmp(&b.3.to_ascii_lowercase()))
        });
        scored.into_iter().map(|(_, _, _, m)| m).collect()
    }

    pub async fn slot_statuses(&self) -> Vec<SlotStatus> {
        let workers = self.workers.lock().await;
        self.plan
            .slots
            .iter()
            .map(|spec| {
                // Missing from the map = temporarily checked out for an in-flight invoke.
                let (busy, healthy, loaded) = workers
                    .get(&spec.id)
                    .map(|w| (w.busy, w.healthy, w.loaded_models.clone()))
                    .unwrap_or((true, true, Vec::new()));
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

    pub async fn idle_slot_count(&self) -> u32 {
        self.idle_slot_ids().await.len() as u32
    }

    pub async fn max_concurrent_jobs(&self) -> u32 {
        // Advertise accelerator parallelism only. Counting CPU made the router
        // claim 3-wide on dual-4GB boxes; overflow onto cpu-0 then OOMs / damages
        // under 8B offload fanout. CPU remains a placement fallback when claimed.
        let workers = self.workers.lock().await;
        let accel = self
            .plan
            .slots
            .iter()
            .filter(|s| s.kind != "cpu")
            .filter(|s| workers.get(&s.id).map(|w| w.healthy).unwrap_or(true))
            .count();
        accel.max(1) as u32
    }

    #[allow(dead_code)]
    pub async fn any_busy(&self) -> bool {
        let workers = self.workers.lock().await;
        workers.values().any(|w| w.busy)
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
    }

    pub async fn warm_models(&self, runtime_models: &[String]) -> Result<()> {
        if runtime_models.is_empty() {
            return Ok(());
        }
        let _mmap = self.lock_mmap_if_tight().await;
        let idle = self.idle_slot_ids().await;
        let has_accel = self.plan.slots.iter().any(|s| s.kind != "cpu");
        // Never fall back to warming cpu-0 while GPUs exist but are busy.
        let targets: Vec<String> = idle
            .into_iter()
            .filter(|id| {
                if id.starts_with("cpu-") {
                    !has_accel
                } else {
                    true
                }
            })
            .collect();
        if targets.is_empty() {
            return Ok(());
        }

        for slot_id in targets {
            let slot_vram = self
                .plan
                .slots
                .iter()
                .find(|s| s.id == slot_id)
                .map(|s| s.card.total_vram_gb)
                .unwrap_or(0);
            // Per-slot pick: demand → fits this card → lightest.
            let ordered = self.order_models_for_warm(runtime_models, slot_vram).await;
            let Some(model) = ordered.first().cloned() else {
                continue;
            };

            let mut workers = self.workers.lock().await;
            let Some(worker) = workers.get_mut(&slot_id) else {
                continue;
            };
            if worker.busy || !worker.healthy {
                continue;
            }
            // Already resident — keep it. Advisory warm must not evict/offload a
            // warm model just to chase a different catalog preference.
            if !worker.loaded_models.is_empty() {
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
        Ok(())
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
        self.record_demand(runtime_model).await;
        let cancel = self.register_job_cancel(job_id).await;
        let _mmap = if self.ram_gb > TIGHT_RAM_GB {
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
            let placement = {
                let mut workers = self.workers.lock().await;
                let idle: Vec<String> = self
                    .plan
                    .slots
                    .iter()
                    .filter(|s| !skip.contains(&s.id))
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
                    crate::protocol::messages_have_images(messages),
                ) {
                    Some(p) => p,
                    None => {
                        self.clear_job_cancel(job_id).await;
                        if let Some(err) = last_crash {
                            return Err(err);
                        }
                        let need_vision = crate::protocol::messages_have_images(messages);
                        let detail =
                            placement_miss_detail(&self.plan, &idle, model, need_vision);
                        return Err(anyhow!(detail));
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
                        bail!("agent_busy: slot {sid} not available");
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
                    return Err(err);
                }
            }
        }

        self.clear_job_cancel(job_id).await;
        Err(last_crash.unwrap_or_else(|| anyhow!("agent_busy: no remaining compute slot")))
    }

    async fn invoke_single(
        &self,
        placement: &Placement,
        job_id: &str,
        model_id: &str,
        runtime_model: &str,
        messages: &[ChatMessage],
        max_tokens: u32,
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
        crate::paths::resolve_agent_binary().unwrap_or_else(|_| {
            std::path::PathBuf::from("scalattice-agent")
        })
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
        Ok(WorkerResponse::Pong { .. }) => Ok(worker),
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
            bail!("worker closed stdout (after {skipped} non-json line(s))");
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
            WorkerResponse::Pong { id }
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
    let mut silence = WORKER_LOAD_SILENCE;
    let mut last_phase = String::from("start");
    let started = Instant::now();
    let mut last_progress = Instant::now();
    loop {
        if started.elapsed() >= WORKER_INVOKE_WALL_CLOCK {
            warn!(
                slot = %worker.spec.id,
                phase = %last_phase,
                wall_s = started.elapsed().as_secs(),
                "killing worker; invoke exceeded wall-clock limit"
            );
            let _ = worker.child.kill().await;
            let _ = worker.child.wait().await;
            worker.healthy = false;
            bail!("invoke_timeout: exceeded wall-clock limit");
        }
        buf.clear();
        let silence_left = silence
            .checked_sub(last_progress.elapsed())
            .unwrap_or(Duration::ZERO);
        let wall_left = WORKER_INVOKE_WALL_CLOCK
            .checked_sub(started.elapsed())
            .unwrap_or(Duration::from_millis(1));
        let wait = silence_left
            .min(wall_left)
            .max(Duration::from_millis(50));
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
                    bail!("worker closed stdout during invoke");
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
                        last_phase = phase.clone();
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
                        last_phase = "decode".to_string();
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
                        bail!("{error}");
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
                bail!("invoke_timeout: worker made no progress");
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
    let d = format!("{err:#}").to_lowercase();
    if d.contains("request_canceled")
        || d.contains("prompt too long")
        || d.contains("invalid_image")
        || d.contains("agent_busy")
        || d.contains("insufficient_vram")
    {
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
    use super::worker_crash_retryable;

    #[test]
    fn stdout_close_retries_on_another_slot() {
        let err = anyhow::anyhow!("worker closed stdout during invoke");
        assert!(worker_crash_retryable(&err));
    }

    #[test]
    fn cancel_and_busy_do_not_retry() {
        assert!(!worker_crash_retryable(&anyhow::anyhow!("request_canceled")));
        assert!(!worker_crash_retryable(&anyhow::anyhow!(
            "agent_busy: no idle compute slot"
        )));
    }
}
