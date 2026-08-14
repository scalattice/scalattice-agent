use super::demand::DemandTracker;
use super::ipc::{WorkerBootConfig, WorkerRequest, WorkerResponse};
use super::placement::{pick_placement, Placement};
use crate::compute_pool::{build_compute_slots, ComputePlan, ComputeSlot};
use crate::protocol::{CatalogModel, ChatMessage, InvokeTimings};
use crate::specs::ComputeDevice;
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};

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

pub struct Hypervisor {
    plan: ComputePlan,
    devices: Vec<ComputeDevice>,
    workers: Mutex<HashMap<String, SlotWorker>>,
    demand: Mutex<DemandTracker>,
    changed: Notify,
    /// In-flight job_id → cancel signal (kill worker when router abandons).
    job_cancels: Mutex<HashMap<String, Arc<Notify>>>,
}

/// Give up only when the worker stops sending progress/token lines.
/// Load/prefill/decode emit those lines while they are actually working.
const WORKER_COMMS_SILENCE: Duration = Duration::from_secs(45);

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
        Ok(Arc::new(Self {
            plan,
            devices: devices.to_vec(),
            workers: Mutex::new(workers),
            demand: Mutex::new(DemandTracker::default()),
            changed: Notify::new(),
            job_cancels: Mutex::new(HashMap::new()),
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
            let req_id = next_req_id();
            let outcome = worker_rpc(
                &mut worker,
                WorkerRequest::Warm {
                    id: req_id,
                    runtime_model: model.clone(),
                },
            )
            .await;
            {
                let mut workers = self.workers.lock().await;
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
                worker.busy = false;
                workers.insert(slot_id, worker);
            }
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
        mut on_delta: Option<Box<dyn FnMut(String) + Send>>,
    ) -> Result<(String, u32, u32, InvokeTimings, String)> {
        self.record_demand(runtime_model).await;
        let cancel = self.register_job_cancel(job_id).await;

        // Atomic pick+claim under one lock — prevents concurrent invokes all
        // selecting the same idle slot (TOCTOU → "slot not available" → damage).
        let placement = {
            let mut workers = self.workers.lock().await;
            let idle: Vec<String> = self
                .plan
                .slots
                .iter()
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
            ) {
                Some(p) => p,
                None => {
                    self.clear_job_cancel(job_id).await;
                    return Err(anyhow!("agent_busy: no idle compute slot for {model_id}"));
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
                    // Roll back busy claims from this placement.
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
        self.clear_job_cancel(job_id).await;
        result
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

        worker.busy = false;
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

        {
            let mut workers = self.workers.lock().await;
            workers.insert(slot_id.clone(), worker);
        }
        self.changed.notify_waiters();
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
        {
            let mut workers = self.workers.lock().await;
            for sid in &placement.slot_ids {
                let Some(w) = workers.get_mut(sid) else {
                    bail!("missing sibling slot {sid}");
                };
                // Stop the process so CVD can be reused by TP worker.
                let _ = w.child.kill().await;
                w.healthy = false;
            }
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
                            debug!(slot = %slot_log_id, "{t}");
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
    loop {
        buf.clear();
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
                        if let Some(cb) = on_delta.as_mut() {
                            cb(format!(
                                "\u{1e}{}\u{1e}{}",
                                phase,
                                pct.unwrap_or(-1.0)
                            ));
                        }
                    }
                    WorkerResponse::Delta { id, text } if id == expect_id => {
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
            _ = tokio::time::sleep(WORKER_COMMS_SILENCE) => {
                warn!(
                    slot = %worker.spec.id,
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
