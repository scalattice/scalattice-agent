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
use tracing::{info, warn};

static REQ_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_req_id() -> String {
    format!("r{}", REQ_SEQ.fetch_add(1, Ordering::Relaxed))
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
        Ok(Arc::new(Self {
            plan,
            devices: devices.to_vec(),
            workers: Mutex::new(workers),
            demand: Mutex::new(DemandTracker::default()),
            changed: Notify::new(),
        }))
    }

    pub fn plan(&self) -> &ComputePlan {
        &self.plan
    }

    pub async fn record_demand(&self, runtime_model: &str) {
        self.demand.lock().await.record_hit(runtime_model);
    }

    pub async fn order_models_by_demand(&self, models: &[String]) -> Vec<String> {
        self.demand
            .lock()
            .await
            .order_by_demand(models, Duration::from_secs(30 * 60))
    }

    pub async fn slot_statuses(&self) -> Vec<SlotStatus> {
        let workers = self.workers.lock().await;
        self.plan
            .slots
            .iter()
            .map(|spec| {
                let (busy, healthy, loaded) = workers
                    .get(&spec.id)
                    .map(|w| (w.busy, w.healthy, w.loaded_models.clone()))
                    .unwrap_or((false, false, Vec::new()));
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
        // CPU counts; accelerator slots are the main parallelism.
        let workers = self.workers.lock().await;
        workers.values().filter(|w| w.healthy).count().max(1) as u32
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
        let ordered = self.order_models_by_demand(runtime_models).await;
        let idle = self.idle_slot_ids().await;
        let workers_guard = self.workers.lock().await;
        // Warm highest-demand model onto each idle accelerator (skip CPU unless only option).
        let accel: Vec<&str> = idle
            .iter()
            .filter(|id| !id.starts_with("cpu-"))
            .map(|s| s.as_str())
            .collect();
        let targets = if accel.is_empty() {
            idle.iter().map(|s| s.as_str()).collect::<Vec<_>>()
        } else {
            accel
        };
        drop(workers_guard);

        for (i, slot_id) in targets.into_iter().enumerate() {
            let Some(model) = ordered.get(i % ordered.len().max(1)) else {
                break;
            };
            // ≤8GB slots: only one warm model.
            let mut workers = self.workers.lock().await;
            let Some(worker) = workers.get_mut(slot_id) else {
                continue;
            };
            if worker.busy || !worker.healthy {
                continue;
            }
            if worker.spec.card.total_vram_gb > 0 && worker.spec.card.total_vram_gb <= 8 {
                if !worker.loaded_models.is_empty()
                    && worker.loaded_models.iter().any(|m| m == model)
                {
                    continue;
                }
                if !worker.loaded_models.is_empty() {
                    let req_id = next_req_id();
                    let _ = worker_rpc(worker, WorkerRequest::Evict { id: req_id }).await;
                    worker.loaded_models.clear();
                }
            }
            let req_id = next_req_id();
            match worker_rpc(
                worker,
                WorkerRequest::Warm {
                    id: req_id,
                    runtime_model: model.clone(),
                },
            )
            .await
            {
                Ok(WorkerResponse::Ok { .. }) => {
                    if !worker.loaded_models.iter().any(|m| m == model) {
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

        let idle = self.idle_slot_ids().await;
        let placement = pick_placement(
            &self.plan,
            &idle,
            model,
            ram_gb,
            cpu_ram_headroom_gb,
            &self.devices,
        )
        .ok_or_else(|| anyhow!("no idle compute slot can host model {model_id}"))?;

        if placement.use_tp_worker {
            self.invoke_tp(
                &placement,
                job_id,
                model_id,
                runtime_model,
                messages,
                max_tokens,
                on_delta.as_mut(),
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
            )
            .await
        }
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
    ) -> Result<(String, u32, u32, InvokeTimings, String)> {
        let slot_id = placement
            .slot_ids
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("empty placement"))?;
        let mut workers = self.workers.lock().await;
        let worker = workers
            .get_mut(&slot_id)
            .ok_or_else(|| anyhow!("slot worker {slot_id} missing"))?;
        if worker.busy || !worker.healthy {
            bail!("slot {slot_id} not available");
        }
        worker.busy = true;
        drop(workers);
        self.changed.notify_waiters();

        let req_id = next_req_id();
        let stream = on_delta.is_some();
        let result = {
            let mut workers = self.workers.lock().await;
            let worker = workers
                .get_mut(&slot_id)
                .ok_or_else(|| anyhow!("slot worker {slot_id} missing"))?;
            let outcome = worker_rpc_invoke(
                worker,
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
            )
            .await;
            worker.busy = false;
            if let Ok((_, _, _, _, loaded)) = &outcome {
                worker.loaded_models = loaded.clone();
            }
            // Detect dead child.
            if let Ok(Some(status)) = worker.child.try_wait() {
                warn!(slot = %slot_id, ?status, "slot worker exited; respawning");
                worker.healthy = false;
                if let Ok(new_w) = spawn_worker(&worker.spec).await {
                    *worker = new_w;
                }
            }
            outcome
        };
        self.changed.notify_waiters();
        result.map(|(c, p, t, timings, _)| (c, p, t, timings, slot_id))
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
    ) -> Result<(String, u32, u32, InvokeTimings, String)> {
        // Pause sibling single-GPU workers, run ephemeral TP worker, then restore.
        let mut workers = self.workers.lock().await;
        for sid in &placement.slot_ids {
            let Some(w) = workers.get_mut(sid) else {
                bail!("missing sibling slot {sid}");
            };
            if w.busy {
                bail!("sibling slot {sid} busy");
            }
            w.busy = true;
            // Stop the process so CVD can be reused by TP worker.
            let _ = w.child.kill().await;
            w.healthy = false;
        }
        drop(workers);

        let tp_key = placement.slot_ids.join("+");
        let tp_spec = ComputeSlot {
            id: format!("tp:{tp_key}"),
            kind: "tensor_parallel".into(),
            priority: 5,
            card: placement.card.clone(),
            cuda_visible: placement.cuda_visible.clone(),
            tp_group: None,
        };

        let mut tp_worker = spawn_worker(&tp_spec)
            .await
            .context("spawn tensor-parallel worker")?;
        tp_worker.busy = true;

        let req_id = next_req_id();
        let stream = on_delta.is_some();
        let outcome = worker_rpc_invoke(
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

        outcome.map(|(c, p, t, timings, _)| (c, p, t, timings, tp_spec.id))
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
        // Keep worker logs out of the IPC pipe (belt-and-braces with stderr logging).
        .env("RUST_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn worker for slot {} ({})", slot.id, bin.display()))?;

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
            WorkerResponse::Delta { .. } => continue,
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

async fn worker_rpc_invoke(
    worker: &mut SlotWorker,
    req: WorkerRequest,
    mut on_delta: Option<&mut Box<dyn FnMut(String) + Send>>,
) -> Result<(String, u32, u32, InvokeTimings, Vec<String>)> {
    let expect_id = request_id(&req);
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    worker.stdin.write_all(line.as_bytes()).await?;
    worker.stdin.flush().await?;

    let mut buf = String::new();
    loop {
        buf.clear();
        let n = worker
            .reader
            .read_line(&mut buf)
            .await
            .context("read worker invoke response")?;
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
