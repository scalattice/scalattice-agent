use crate::config::{read_saved_agent_token, token_snippet, AgentConfig, SCALATTICE_WS_URL};
use crate::protocol::{
    parse_envelope, parse_error, parse_invoke, parse_invoke_cancel, parse_invoke_split, parse_pong, parse_ready, parse_registered,
    AgentSchedule, CatalogModel, ComputeDevicePolicy, ControlAckMessage, ControlMessage, HeartbeatMessage,
    InvokeDeltaMessage, InvokeErrorMessage, InvokeResultMessage, LogsBatchMessage, LogsLinePayload,
    LogsSubscribeMessage, ModelPolicyEntry, RegisterMessage,
};
use crate::vram_lifecycle::{ScheduleTransition, VramLifecycleConfig, VramLifecycleState, VramTickAction};
use crate::hypervisor::{Hypervisor, SlotStatus};
use crate::inference::InferenceEngine;
use crate::models::{
    can_host_on_machine, handle_weight_load_failure,
    preferred_download_card, purge_incomplete_model_weights,
    should_skip_preload, spawn_delete_staged_dirs, stage_purge_model_weights,
    sweep_staged_purge_dirs, spawn_catalog_sync,
};
use crate::runtime::{build_runtime, JobState};
use crate::specs::{
    apply_compute_policy, build_specs_from_devices, detect_all_compute_devices, detect_cpu_model,
    detect_cuda_version, detect_driver_version, detect_hostname, detect_machine_specs, detect_ram_gb,
    MachineSpecs,
};
use crate::state;
use anyhow::{anyhow, bail, Context, Result};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{interval, MissedTickBehavior};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

type WsWrite = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type SharedWsWrite = Arc<Mutex<WsWrite>>;

/// Full Windows specs detection (PowerShell + nvidia-smi) is expensive. Cache it so the
/// WebSocket loop is not blocked for tens of seconds every heartbeat / invoke.
const SPECS_CACHE_TTL: Duration = Duration::from_secs(20);

struct SessionState {
    registered: bool,
    compute_policy: Vec<(String, bool)>,
    model_policy: Vec<(String, bool)>,
    /// Platform-wide completion ceiling from policy (same on every machine).
    max_completion_tokens: u32,
    /// Server-controlled RAM headroom for CPU / offload fit (from ready).
    cpu_ram_headroom_gb: u32,
    job_state: JobState,
    active_job_id: Option<String>,
    active_model_id: Option<String>,
    active_job_count: u32,
    advertised_models: Vec<String>,
    node_id: Option<String>,
    catalog: Vec<CatalogModel>,
    hf_token: Option<String>,
    last_sync_token: Option<String>,
    download_cancel: Arc<AtomicBool>,
    sync_in_flight: Arc<AtomicBool>,
    logged_download_blockers: bool,
    vram_lifecycle: VramLifecycleState,
    /// Cached machine specs. RefCell: SessionState is only accessed while holding the tokio Mutex.
    specs_cache: RefCell<Option<(Instant, MachineSpecs)>>,
    hypervisor: Option<Arc<Hypervisor>>,
    cached_slots: Vec<SlotStatus>,
    cached_idle_slots: u32,
    cached_max_jobs: u32,
    cached_loaded_models: Vec<String>,
    pending_hypervisor_restart: bool,
    /// Disk inventory for heartbeats. Refreshed off the WS thread — walking GGUFs
    /// inline stalls invoke/log frames for tens of seconds on some machines.
    disk_inventory_primed: bool,
    cached_disk_ready: Vec<String>,
    cached_disk_gb: u32,
    cached_model_disk: Vec<(String, crate::models::ModelDiskStatus)>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            registered: false,
            compute_policy: Vec::new(),
            model_policy: Vec::new(),
            max_completion_tokens: 1024,
            cpu_ram_headroom_gb: crate::models::DEFAULT_CPU_RAM_HEADROOM_GB,
            job_state: JobState::Idle,
            active_job_id: None,
            active_model_id: None,
            active_job_count: 0,
            advertised_models: Vec::new(),
            node_id: None,
            catalog: Vec::new(),
            hf_token: None,
            last_sync_token: None,
            download_cancel: Arc::new(AtomicBool::new(false)),
            sync_in_flight: Arc::new(AtomicBool::new(false)),
            logged_download_blockers: false,
            vram_lifecycle: VramLifecycleState::default(),
            specs_cache: RefCell::new(None),
            hypervisor: None,
            cached_slots: Vec::new(),
            cached_idle_slots: 0,
            cached_max_jobs: 1,
            cached_loaded_models: Vec::new(),
            pending_hypervisor_restart: false,
            disk_inventory_primed: false,
            cached_disk_ready: Vec::new(),
            cached_disk_gb: 0,
            cached_model_disk: Vec::new(),
        }
    }

    fn disk_has_runtime(&self, runtime_or_id: &str) -> bool {
        let want = runtime_or_id.trim();
        if want.is_empty() {
            return false;
        }
        self.cached_disk_ready.iter().any(|id| id.eq_ignore_ascii_case(want))
    }

    fn catalog_ready_on_disk(&self, model: &CatalogModel) -> bool {
        if !self.disk_inventory_primed {
            // Unprimed: skip GGUF walks on the WS thread. Inventory fills this in
            // spawn_blocking; the agent re-registers once the scan lands.
            return false;
        }
        let runtime = if model.runtime_model.trim().is_empty() {
            model.model_id.as_str()
        } else {
            model.runtime_model.as_str()
        };
        self.disk_has_runtime(runtime) || self.disk_has_runtime(&model.model_id)
    }

    fn vram_config(&self) -> VramLifecycleConfig {
        VramLifecycleConfig::from_env()
    }

    fn evict_vram_cache(&self) {
        info!("evicting in-memory model weights from VRAM");
        if let Some(hv) = &self.hypervisor {
            let hv = hv.clone();
            tokio::spawn(async move {
                hv.evict_all().await;
            });
        } else {
            crate::llm::evict_all();
        }
    }

    fn apply_max_completion_tokens(&mut self, raw: u32) {
        let next = if raw == 0 {
            1024
        } else {
            raw.clamp(16, 8192)
        };
        if self.max_completion_tokens != next {
            info!(max_completion_tokens = next, "updated platform completion token ceiling");
            self.max_completion_tokens = next;
        }
    }

    fn effective_max_tokens(&self, requested: u32) -> u32 {
        let req = if requested == 0 { 1024 } else { requested };
        req.min(self.max_completion_tokens).clamp(1, 8192)
    }

    fn apply_schedule(&mut self, schedule: AgentSchedule) -> ScheduleTransition {
        let transition = self.vram_lifecycle.apply_schedule(schedule);
        if transition.left_earning {
            self.evict_vram_cache();
        }
        transition
    }

    fn tick_vram_lifecycle(&mut self) {
        let config = self.vram_config();
        let action = self.vram_lifecycle.tick(self.job_state, &config);
        if action == VramTickAction::EvictVram {
            self.evict_vram_cache();
        }
    }

    fn warm_runtime_models(&self) -> Vec<String> {
        let models = self
            .register_model_ids()
            .into_iter()
            .filter_map(|model_id| {
                self.catalog.iter().find_map(|model| {
                    if model.model_id != model_id {
                        return None;
                    }
                    let runtime = if model.runtime_model.trim().is_empty() {
                        model.model_id.clone()
                    } else {
                        model.runtime_model.clone()
                    };
                    if should_skip_preload(&runtime) {
                        return None;
                    }
                    Some(runtime)
                })
            })
            .collect::<Vec<_>>();
        // Demand ordering + one-resident-per-small-slot happens in warm_models.
        models
    }

    fn effective_hf_token(&self, server_token: Option<String>) -> Option<String> {
        server_token
            .or_else(|| self.hf_token.clone())
            .or_else(|| std::env::var("SCALATTICE_HF_TOKEN").ok())
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
    }

    fn pending_weight_downloads(&self) -> Vec<CatalogModel> {
        self.eligible_catalog_models()
            .into_iter()
            .filter(|model| model.weights.is_some())
            .filter(|model| !self.catalog_ready_on_disk(model))
            .collect()
    }

    fn needs_reregister(&self) -> bool {
        self.register_model_ids() != self.advertised_models
    }

    fn sync_model_weights(&mut self, server_token: Option<String>, agent_token: &str) {
        let pending = self.pending_weight_downloads();
        if pending.is_empty() {
            if !self.logged_download_blockers {
                self.log_download_blockers();
                self.logged_download_blockers = true;
            }
            return;
        }
        if self.sync_in_flight.load(Ordering::Relaxed) {
            return;
        }
        let token = self.effective_hf_token(server_token);
        let can_mirror = pending.iter().any(|m| {
            m.weights
                .as_ref()
                .and_then(|w| w.mirror_url.as_deref())
                .is_some_and(|url| !url.trim().is_empty())
        });
        if token.is_none() && !can_mirror {
            warn!("model downloads are not configured on the server yet (contact Scalattice support)");
            return;
        }
        self.last_sync_token = token.clone();
        if let Some(token) = token.clone() {
            self.hf_token = Some(token);
        }
        let specs = self.enabled_devices();
        let ram_gb = specs.ram_gb.or(detect_ram_gb()).unwrap_or(0);
        let card = match preferred_download_card(&specs.compute_devices) {
            Ok(card) => card,
            Err(err) => {
                warn!("model downloads skipped: {err:#}");
                return;
            }
        };
        info!(
            "starting model weight downloads for {} eligible model(s)",
            pending.len()
        );
        let enabled_ids: std::collections::HashSet<String> = pending
            .iter()
            .map(|model| model.model_id.clone())
            .collect();
        self.sync_in_flight.store(true, Ordering::Relaxed);
        spawn_catalog_sync(
            pending,
            card,
            ram_gb,
            self.cpu_ram_headroom_gb,
            agent_token.to_string(),
            token,
            self.download_cancel.clone(),
            self.sync_in_flight.clone(),
            enabled_ids,
        );
    }

    fn log_download_blockers(&self) {
        let enabled: Vec<&CatalogModel> = self
            .catalog
            .iter()
            .filter(|model| self.is_model_enabled(&model.model_id))
            .filter(|model| model.weights.is_some())
            .collect();
        if enabled.is_empty() {
            return;
        }
        let specs = self.enabled_devices();
        let ram_gb = specs.ram_gb.or(detect_ram_gb()).unwrap_or(0);
        for model in enabled {
            if self.catalog_ready_on_disk(model) {
                continue;
            }
            if !can_host_on_machine(model, &specs.compute_devices, ram_gb, self.cpu_ram_headroom_gb)
            {
                warn!(
                    "model {} cannot run on this machine (needs {} GB VRAM / {} GB RAM; machine has {} GB RAM)",
                    model.model_id,
                    model.min_vram_gb.unwrap_or(0.0),
                    model.min_ram_gb.unwrap_or(0.0),
                    ram_gb
                );
            }
        }
    }

    fn cancel_active_downloads(&mut self) {
        self.download_cancel.store(true, Ordering::Relaxed);
        self.download_cancel = Arc::new(AtomicBool::new(false));
        self.sync_in_flight.store(false, Ordering::Relaxed);
        state::set_downloading_model(None);
    }

    fn prune_disabled_model_weights(&self) {
        for model in &self.catalog {
            if self.is_model_enabled(&model.model_id) {
                continue;
            }
            let runtime_model = if model.runtime_model.trim().is_empty() {
                model.model_id.as_str()
            } else {
                model.runtime_model.as_str()
            };
            purge_incomplete_model_weights(runtime_model);
        }
    }

    fn apply_model_policy(&mut self, models: &[ModelPolicyEntry]) {
        if models.is_empty() {
            return;
        }
        let next: Vec<(String, bool)> = models
            .iter()
            .map(|model| (model.model_id.clone(), model.enabled))
            .collect();
        if self.model_policy == next {
            return;
        }

        self.model_policy = next;
        self.logged_download_blockers = false;
        self.prune_disabled_model_weights();

        if let Some(downloading) = crate::state::downloading_model() {
            let still_enabled = self
                .model_policy
                .iter()
                .any(|(id, enabled)| id == &downloading && *enabled);
            if !still_enabled {
                self.cancel_active_downloads();
            }
        }
    }

    fn runtime_for_model_id(&self, model_id: &str) -> String {
        self.catalog
            .iter()
            .find(|model| model.model_id == model_id)
            .map(|model| {
                if model.runtime_model.trim().is_empty() {
                    model.model_id.clone()
                } else {
                    model.runtime_model.clone()
                }
            })
            .unwrap_or_else(|| model_id.replace("__", "/"))
    }

    fn apply_purge_models(&mut self, model_ids: &[String]) -> Vec<std::path::PathBuf> {
        if model_ids.is_empty() {
            return Vec::new();
        }
        if let Some(downloading) = crate::state::downloading_model() {
            if model_ids.iter().any(|id| id == &downloading) {
                self.cancel_active_downloads();
            }
        }
        let mut trash = Vec::new();
        for model_id in model_ids {
            let runtime_model = self.runtime_for_model_id(model_id);
            info!("purging model weights for {model_id} ({runtime_model})");
            if let Some(path) = stage_purge_model_weights(&runtime_model) {
                trash.push(path);
            }
        }
        trash
    }

    fn is_model_enabled(&self, model_id: &str) -> bool {
        if self.model_policy.is_empty() {
            return true;
        }
        self.model_policy
            .iter()
            .find(|(id, _)| id == model_id)
            .map(|(_, enabled)| *enabled)
            .unwrap_or(false)
    }

    fn eligible_catalog_models(&self) -> Vec<CatalogModel> {
        let specs = self.enabled_devices();
        let ram_gb = specs.ram_gb.or(detect_ram_gb()).unwrap_or(0);

        self.catalog
            .iter()
            .filter(|model| self.is_model_enabled(&model.model_id))
            .filter(|model| {
                can_host_on_machine(
                    model,
                    &specs.compute_devices,
                    ram_gb,
                    self.cpu_ram_headroom_gb,
                )
            })
            .cloned()
            .collect()
    }

    fn register_model_ids(&self) -> Vec<String> {
        self.eligible_catalog_models()
            .iter()
            .filter(|model| self.catalog_ready_on_disk(model))
            .map(|m| m.model_id.clone())
            .collect()
    }

    fn apply_compute_devices(&mut self, devices: &[ComputeDevicePolicy]) {
        if devices.is_empty() {
            return;
        }
        let mut next: Vec<(String, bool)> = devices
            .iter()
            .map(|device| (device.id.clone(), device.enabled))
            .collect();
        // Order-independent compare — server may reshuffle the same set.
        next.sort_by(|a, b| a.0.cmp(&b.0));
        let mut current = self.compute_policy.clone();
        current.sort_by(|a, b| a.0.cmp(&b.0));
        if current == next {
            return;
        }
        let enabled = next.iter().filter(|(_, on)| *on).count();
        info!(
            enabled,
            total = next.len(),
            "dashboard compute device policy updated"
        );
        if enabled == 0 {
            warn!(
                "{} compute device(s) detected (none enabled in dashboard)",
                next.len()
            );
        }
        self.compute_policy = next;
        self.invalidate_specs_cache();
        self.pending_hypervisor_restart = true;
    }

    fn invalidate_specs_cache(&self) {
        *self.specs_cache.borrow_mut() = None;
    }

    fn store_specs_cache(&self, specs: MachineSpecs) {
        *self.specs_cache.borrow_mut() = Some((Instant::now(), specs));
    }

    fn cached_specs(&self) -> Option<MachineSpecs> {
        self.specs_cache
            .borrow()
            .as_ref()
            .map(|(_, specs)| specs.clone())
    }

    fn specs_cache_fresh(&self) -> bool {
        matches!(
            self.specs_cache.borrow().as_ref(),
            Some((at, _)) if at.elapsed() < SPECS_CACHE_TTL
        )
    }

    fn detect_enabled_devices(policy: &[(String, bool)]) -> MachineSpecs {
        let mut devices = detect_all_compute_devices();
        apply_compute_policy(&mut devices, policy);
        build_specs_from_devices(
            &devices,
            detect_hostname(),
            detect_cpu_model(),
            detect_ram_gb(),
            detect_driver_version(),
            detect_cuda_version(),
        )
    }

    fn enabled_devices(&self) -> MachineSpecs {
        // Prefer any cached snapshot (even slightly stale) over blocking the WS loop
        // with PowerShell/nvidia-smi. Heartbeat refreshes the cache off-thread.
        if let Some(specs) = self.cached_specs() {
            return specs;
        }
        let specs = Self::detect_enabled_devices(&self.compute_policy);
        self.store_specs_cache(specs.clone());
        specs
    }

    fn live_specs(&self) -> MachineSpecs {
        let mut specs = self.enabled_devices();
        if let Some(slot) = self
            .cached_slots
            .iter()
            .filter(|s| s.kind != "cpu" && s.healthy)
            .max_by_key(|s| s.vram_gb)
        {
            specs.gpu_name = Some(slot.display_name.clone());
            specs.vram_gb = Some(slot.vram_gb);
        } else if self.cached_slots.len() > 1 {
            let total: u32 = self.cached_slots.iter().map(|s| s.vram_gb).sum();
            if total > 0 {
                specs.vram_gb = Some(total);
                specs.gpu_name = Some(format!("{} compute slots", self.cached_slots.len()));
            }
        }
        specs
    }

    fn count_blocked_enabled_models(&self) -> usize {
        let specs = self.enabled_devices();
        let ram_gb = specs.ram_gb.unwrap_or(0);
        let enabled_models: Vec<&CatalogModel> = self
            .catalog
            .iter()
            .filter(|model| self.is_model_enabled(&model.model_id))
            .filter(|model| model.weights.is_some())
            .collect();
        if enabled_models.is_empty() {
            return 0;
        }
        if specs.compute_devices.iter().all(|d| !d.enabled) {
            return enabled_models.len();
        }

        enabled_models
            .into_iter()
            .filter(|model| {
                !can_host_on_machine(
                    model,
                    &specs.compute_devices,
                    ram_gb,
                    self.cpu_ram_headroom_gb,
                )
            })
            .count()
    }

    fn runtime(&self) -> crate::runtime::AgentRuntime {
        let specs = self.enabled_devices();
        // Always report disk-cached weights here. VRAM-resident models live on
        // slot status; substituting them made the dashboard flicker to "only the
        // running model is installed" under load.
        let loaded_models = if self.disk_inventory_primed {
            self.cached_disk_ready.clone()
        } else {
            // Never walk GGUFs on the WS/heartbeat path; inventory refreshes in the
            // background. Empty until the first off-thread scan completes.
            Vec::new()
        };
        let enabled_count = specs.compute_devices.iter().filter(|d| d.enabled).count();
        let downloading = crate::state::downloading_model();
        let blocked_models = if downloading.is_some() || !loaded_models.is_empty() {
            0
        } else if self.pending_weight_downloads().is_empty() {
            self.count_blocked_enabled_models()
        } else {
            0
        };

        build_runtime(
            self.job_state,
            self.active_job_id.clone(),
            self.active_model_id.clone(),
            &loaded_models,
            enabled_count,
            downloading.as_deref(),
            blocked_models,
            self.cached_disk_gb,
            crate::runtime::serialize_model_disk(&self.cached_model_disk),
            self.cached_slots.clone(),
            self.cached_max_jobs.max(1),
            self.cached_idle_slots,
        )
    }

    fn persist_local_state(&self) {
        let runtime = self.runtime();
        let devices = self.enabled_devices().compute_devices;
        state::update_connection_state(
            Some(runtime.status_label),
            runtime.downloading_model.clone(),
            self.node_id.clone(),
            true,
            self.registered,
            None,
            devices,
        );
    }
}

const TOKEN_UPDATED: &str = "token_updated";
/// Fixed short delay only - no exponential backoff. Tiny pause so a dead server
/// cannot pin a core in a tight reconnect loop.
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

pub async fn run_agent(mut config: AgentConfig) -> Result<()> {
    // Supervisor never touches CUDA — per-slot workers set CUDA_VISIBLE_DEVICES themselves.
    let specs = detect_machine_specs();
    info!("{}", crate::specs::status_line(&specs));
    sweep_staged_purge_dirs();

    let hypervisor = Hypervisor::start(&specs.compute_devices)
        .await
        .context("start compute hypervisor")?;
    info!(
        slots = hypervisor.plan().slots.len(),
        "compute hypervisor online"
    );

    loop {
        if let Some(fresh) = read_saved_agent_token() {
            if fresh != config.token {
                info!(
                    "provider token changed on disk, using {}",
                    token_snippet(&fresh)
                );
                config.token = fresh;
            }
        }

        match run_agent_session(&config, hypervisor.clone()).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                let err_str = format!("{err:#}");
                if err_str == TOKEN_UPDATED {
                    if let Some(fresh) = read_saved_agent_token() {
                        config.token = fresh;
                    }
                    state::mark_disconnected(Some("reconnecting with new token".to_string()));
                    info!(
                        "reconnecting with provider token {}",
                        token_snippet(&config.token)
                    );
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    continue;
                }
                state::mark_disconnected(Some(err_str.clone()));
                if is_token_auth_error(&err_str) {
                    warn!(
                        "disconnected: {err_str} (token {}); reconnecting...",
                        token_snippet(&config.token)
                    );
                    if let Some(fresh) = read_saved_agent_token() {
                        if fresh != config.token {
                            info!(
                                "retrying with saved token {}",
                                token_snippet(&fresh)
                            );
                            config.token = fresh;
                            continue;
                        }
                    }
                } else {
                    warn!("disconnected: {err_str}; reconnecting...");
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
}

fn is_token_auth_error(message: &str) -> bool {
    message.contains("invalid_agent_token")
        || message.contains("token_revoked")
        || message.contains("invalid agent token")
}

async fn run_agent_session(config: &AgentConfig, hypervisor: Arc<Hypervisor>) -> Result<()> {
    struct StreamingGuard;
    impl Drop for StreamingGuard {
        fn drop(&mut self) {
            crate::cloud_log::set_streaming(false);
        }
    }
    let _streaming_guard = StreamingGuard;

    info!(
        "connecting with provider token {}",
        token_snippet(&config.token)
    );
    let state = Arc::new(Mutex::new(SessionState::new()));
    {
        let mut guard = state.lock().await;
        guard.hypervisor = Some(hypervisor.clone());
    }
    refresh_slot_cache(&state).await;

    let mut request = SCALATTICE_WS_URL
        .into_client_request()
        .context("invalid WebSocket URL")?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {}", config.token).parse()?);

    let (ws, _) = connect_async(request)
        .await
        .context("WebSocket connect failed (check token and network)")?;

    {
        let mut guard = state.lock().await;
        guard.node_id = None;
        guard.registered = false;
        guard.persist_local_state();
    }

    let (write, mut read) = ws.split();
    let write = Arc::new(Mutex::new(write));
    // Prime specs once before the read loop so the first heartbeat/invoke is cheap.
    refresh_specs_cache(&state).await;
    // Disk inventory is slow (GGUF bounds checks). Do it off-thread so the first
    // invoke is not queued behind a cache walk.
    {
        let state_disk = state.clone();
        tokio::spawn(async move {
            refresh_disk_inventory(&state_disk).await;
        });
    }
    let mut heartbeat = interval(Duration::from_secs(12));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut logs_flush = interval(Duration::from_secs(1));
    logs_flush.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut token_poll = interval(Duration::from_secs(1));
    token_poll.tick().await;
    // Only one background maintenance pass at a time so warm/refresh cannot
    // stack and starve the WS select loop (Cloudflare idle ~100s).
    let maintenance_busy = Arc::new(AtomicBool::new(false));

    loop {
        tokio::select! {
            msg = read.next() => {
                let Some(msg) = msg else {
                    bail!("connection closed by server");
                };
                let msg = msg.context("websocket read error")?;
                if !handle_server_message(config, &state, &write, msg).await? {
                    break;
                }
            }
            _ = token_poll.tick() => {
                if let Some(fresh) = read_saved_agent_token() {
                    if fresh != config.token {
                        info!(
                            "provider token saved, reconnecting with {}",
                            token_snippet(&fresh)
                        );
                        bail!(TOKEN_UPDATED);
                    }
                }
            }
            _ = logs_flush.tick() => {
                if crate::cloud_log::is_streaming() {
                    let _ = flush_live_logs(&write).await;
                }
            }
            _ = heartbeat.tick() => {
                let registered = {
                    let mut guard = state.lock().await;
                    guard.tick_vram_lifecycle();
                    guard.registered
                };
                if registered {
                    // Keepalive frame FIRST — never await warm/hypervisor work before
                    // putting bytes on the wire (edge proxies idle-close ~100s).
                    send_heartbeat(&state, &write).await?;
                    if maintenance_busy
                        .compare_exchange(
                            false,
                            true,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        let state_bg = state.clone();
                        let write_bg = write.clone();
                        let token = config.token.clone();
                        let busy = maintenance_busy.clone();
                        tokio::spawn(async move {
                            let _guard = MaintenanceGuard(busy);
                            {
                                let mut guard = state_bg.lock().await;
                                let hf_token = guard.hf_token.clone();
                                guard.sync_model_weights(hf_token, &token);
                            }
                            refresh_specs_cache(&state_bg).await;
                            refresh_slot_cache(&state_bg).await;
                            refresh_disk_inventory(&state_bg).await;
                            maybe_warm_models(state_bg.clone()).await;
                            let reregister = state_bg.lock().await.needs_reregister();
                            if reregister {
                                if let Err(err) = send_register_message(&state_bg, &write_bg).await {
                                    debug!("background re-register failed: {err:#}");
                                }
                            }
                        });
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                return Ok(());
            }
        }
    }

    Ok(())
}

struct MaintenanceGuard(Arc<AtomicBool>);
impl Drop for MaintenanceGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

async fn refresh_slot_cache(state: &Arc<Mutex<SessionState>>) {
    // Restart workers when provider toggles devices.
    // Claim the pending flag immediately so concurrent refresh callers don't
    // start a second Hypervisor::start in parallel.
    let policy = {
        let mut guard = state.lock().await;
        if !(guard.pending_hypervisor_restart && guard.active_job_count == 0) {
            None
        } else {
            guard.pending_hypervisor_restart = false;
            Some(guard.compute_policy.clone())
        }
    };
    if let Some(policy) = policy {
        let specs = tokio::task::spawn_blocking(move || {
            SessionState::detect_enabled_devices(&policy)
        })
        .await;
        if let Ok(specs) = specs {
            match Hypervisor::start(&specs.compute_devices).await {
                Ok(hv) => {
                    let mut guard = state.lock().await;
                    guard.hypervisor = Some(hv);
                    info!("compute hypervisor restarted after device policy change");
                }
                Err(err) => {
                    warn!("hypervisor restart failed: {err:#}");
                    // Allow a later refresh to retry.
                    state.lock().await.pending_hypervisor_restart = true;
                }
            }
        } else {
            state.lock().await.pending_hypervisor_restart = true;
        }
    }

    let hv = state.lock().await.hypervisor.clone();
    let Some(hv) = hv else {
        return;
    };
    let slots = hv.slot_statuses().await;
    let idle = hv.idle_slot_count().await;
    let max = hv.max_concurrent_jobs().await;
    let loaded = hv.loaded_models_union().await;
    let mut guard = state.lock().await;
    guard.cached_slots = slots;
    guard.cached_idle_slots = idle;
    guard.cached_max_jobs = max;
    guard.cached_loaded_models = loaded;
}

async fn refresh_specs_cache(state: &Arc<Mutex<SessionState>>) {
    let (policy, need_refresh) = {
        let guard = state.lock().await;
        let need = !guard.specs_cache_fresh();
        (guard.compute_policy.clone(), need)
    };
    if !need_refresh {
        return;
    }
    let specs = match tokio::task::spawn_blocking(move || {
        SessionState::detect_enabled_devices(&policy)
    })
    .await
    {
        Ok(specs) => specs,
        Err(err) => {
            warn!("specs refresh task failed: {err:#}");
            return;
        }
    };
    state.lock().await.store_specs_cache(specs);
}

async fn maybe_warm_models(state: Arc<Mutex<SessionState>>) {
    let (should_preload, warm_models, hv) = {
        let guard = state.lock().await;
        let config = guard.vram_config();
        if !guard.vram_lifecycle.should_preload(&config) {
            return;
        }
        let warm_models = guard.warm_runtime_models();
        if warm_models.is_empty() {
            return;
        }
        let Some(hv) = guard.hypervisor.clone() else {
            return;
        };
        (true, warm_models, hv)
    };
    if !should_preload {
        return;
    }
    let state_for_task = state.clone();
    tokio::spawn(async move {
        if hv.warm_models(&warm_models).await.is_ok() {
            let mut guard = state_for_task.lock().await;
            guard.vram_lifecycle.on_vram_loaded();
        }
        refresh_slot_cache(&state_for_task).await;
    });
}

async fn send_register_message(state: &Arc<Mutex<SessionState>>, write: &SharedWsWrite) -> Result<()> {
    // Do not await hypervisor restart here — that can stall the WS loop past edge
    // idle timeouts. Slot cache is refreshed on the background maintenance path.
    let register = {
        let mut guard = state.lock().await;
        let models = guard.register_model_ids();
        guard.advertised_models = models.clone();
        let specs = guard.live_specs();
        let runtime = guard.runtime();
        RegisterMessage {
            kind: "register",
            models,
            gpu_name: specs.gpu_name.clone(),
            vram_gb: specs.vram_gb,
            specs: Some(specs),
            runtime: Some(runtime),
        }
    };
    write
        .lock()
        .await
        .send(Message::Text(serde_json::to_string(&register)?))
        .await?;
    Ok(())
}

async fn send_heartbeat(state: &Arc<Mutex<SessionState>>, write: &SharedWsWrite) -> Result<()> {
    // Intentionally light: keepalive must not await hypervisor/warm work.
    let (specs, runtime) = {
        let guard = state.lock().await;
        (guard.live_specs(), guard.runtime())
    };
    let hb = serde_json::to_string(&HeartbeatMessage {
        kind: "heartbeat",
        specs: Some(specs),
        runtime: Some(runtime),
    })?;
    write.lock().await.send(Message::Text(hb)).await?;
    Ok(())
}

async fn refresh_disk_inventory(state: &Arc<Mutex<SessionState>>) {
    let snap = tokio::task::spawn_blocking(|| {
        (
            crate::models::list_cached_runtime_models(),
            crate::models::models_cache_disk_gb(),
            crate::models::list_model_disk_status(),
        )
    })
    .await;
    let Ok((ready, disk_gb, model_disk)) = snap else {
        return;
    };
    let mut guard = state.lock().await;
    guard.cached_disk_ready = ready;
    guard.cached_disk_gb = disk_gb;
    guard.cached_model_disk = model_disk;
    guard.disk_inventory_primed = true;
    crate::state::set_disk_full(crate::specs::disk_is_full());
    guard.persist_local_state();
}

/// Best-effort WS write. Returns Ok even if the peer already reset — invoke tasks
/// must not treat a dying socket as a logic failure that races the reconnect loop.
async fn ws_send_text(write: &SharedWsWrite, text: &str) -> Result<()> {
    match write.lock().await.send(Message::Text(text.to_string())).await {
        Ok(()) => Ok(()),
        Err(err) => {
            let msg = err.to_string().to_lowercase();
            if msg.contains("closed")
                || msg.contains("reset")
                || msg.contains("broken pipe")
                || msg.contains("connection")
            {
                debug!("websocket write skipped (connection closing): {err}");
                Ok(())
            } else {
                Err(err.into())
            }
        }
    }
}

async fn handle_server_message(
    config: &AgentConfig,
    state: &Arc<Mutex<SessionState>>,
    write: &SharedWsWrite,
    msg: Message,
) -> Result<bool> {
    match msg {
        Message::Text(text) => {
            let data = text.as_bytes();
            let env = parse_envelope(data)?;
            match env.kind.as_str() {
                "ready" => {
                    let ready = parse_ready(data)?;
                    info!("assigned node {}", ready.node_id);
                    {
                        let mut guard = state.lock().await;
                        guard.node_id = Some(ready.node_id.clone());
                        guard.apply_compute_devices(&ready.compute_devices);
                        guard.apply_model_policy(&ready.enabled_models);
                        guard.apply_max_completion_tokens(ready.max_completion_tokens);
                        guard.cpu_ram_headroom_gb = ready.cpu_ram_headroom_gb;
                        guard.catalog = ready.catalog.clone();
                        guard.last_sync_token = None;
                        let transition = guard.apply_schedule(ready.schedule.clone());
                        guard.sync_model_weights(ready.hugging_face_token.clone(), &config.token);
                        guard.persist_local_state();
                        drop(guard);
                        if transition.entered_earning {
                            maybe_warm_models(state.clone()).await;
                        }
                    }
                    // Register immediately so the session stays alive; restart workers
                    // and warm in the background (can take tens of seconds).
                    send_register_message(state, write).await?;
                    let state_bg = state.clone();
                    let write_bg = write.clone();
                    tokio::spawn(async move {
                        refresh_disk_inventory(&state_bg).await;
                        refresh_slot_cache(&state_bg).await;
                        maybe_warm_models(state_bg.clone()).await;
                        if state_bg.lock().await.needs_reregister() {
                            if let Err(err) = send_register_message(&state_bg, &write_bg).await {
                                debug!("post-ready re-register failed: {err:#}");
                            }
                        }
                    });
                }
                "registered" => {
                    let reg = parse_registered(data)?;
                    {
                        let mut guard = state.lock().await;
                        guard.registered = true;
                        guard.node_id = Some(reg.node_id.clone());
                        guard.advertised_models = reg.models.clone();
                        guard.persist_local_state();
                    }
                    let runtime = state.lock().await.runtime();
                    info!(
                        "registered · node {} · models: {} · {}",
                        reg.node_id,
                        reg.models.join(", "),
                        runtime.status_label
                    );
                }
                "invoke" => {
                    let invoke = parse_invoke(data)?;
                    let state = state.clone();
                    let write = write.clone();
                    tokio::spawn(async move {
                        if let Err(err) = respond_invoke(&state, &write, invoke).await {
                            warn!("invoke task failed: {err:#}");
                            let code = invoke_error_code(&err);
                            if code != "agent_busy" && code != "request_canceled" {
                                state::record_inference_failure(code, &format!("{err:#}"));
                            }
                        }
                    });
                }
                "invoke_cancel" => {
                    if let Ok(msg) = parse_invoke_cancel(data) {
                        let hv = state.lock().await.hypervisor.clone();
                        if let Some(hv) = hv {
                            if hv.cancel_invoke(&msg.id).await {
                                info!("canceled in-flight invoke {}", msg.id);
                            } else {
                                debug!("invoke_cancel for unknown/finished job {}", msg.id);
                            }
                        }
                    }
                }
                "invoke_split" => {
                    let invoke = parse_invoke_split(data)?;
                    let state = state.clone();
                    let write = write.clone();
                    tokio::spawn(async move {
                        if let Err(err) = respond_invoke_split(&state, &write, invoke).await {
                            warn!("invoke_split task failed: {err:#}");
                            let code = invoke_error_code(&err);
                            if code != "agent_busy" {
                                state::record_inference_failure(code, &format!("{err:#}"));
                            }
                        }
                    });
                }
                "pong" | "policy" => {
                    if let Ok(pong) = parse_pong(data) {
                        let purge_requested = !pong.purge_models.is_empty();
                        let (transition, trash) = {
                            let mut guard = state.lock().await;
                            guard.apply_compute_devices(&pong.compute_devices);
                            guard.apply_model_policy(&pong.enabled_models);
                            guard.apply_max_completion_tokens(pong.max_completion_tokens);
                            let trash = guard.apply_purge_models(&pong.purge_models);
                            let transition = guard.apply_schedule(pong.schedule.clone());
                            guard.sync_model_weights(pong.hugging_face_token.clone(), &config.token);
                            guard.tick_vram_lifecycle();
                            guard.persist_local_state();
                            (transition, trash)
                        };
                        spawn_delete_staged_dirs(trash);
                        if transition.entered_earning {
                            maybe_warm_models(state.clone()).await;
                        }
                        if state.lock().await.needs_reregister() {
                            send_register_message(state, write).await?;
                        } else if purge_requested {
                            send_heartbeat(state, write).await?;
                        }
                    }
                }
                "error" => {
                    let err = parse_error(data)?;
                    let message = err
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    if is_token_auth_error(message) {
                        return Err(anyhow!(
                            "server error: {message} (token {})",
                            token_snippet(&config.token)
                        ));
                    }
                    return Err(anyhow!("server error: {message}"));
                }
                "logs_subscribe" => {
                    if let Ok(msg) = serde_json::from_slice::<LogsSubscribeMessage>(data) {
                        handle_logs_subscribe(&write, &msg.action).await?;
                    }
                }
                "control" => {
                    if let Ok(msg) = serde_json::from_slice::<ControlMessage>(data) {
                        handle_remote_control(&write, &msg.action).await?;
                    }
                }
                other => {
                    info!("ignored message type: {other}");
                }
            }
        }
        Message::Ping(payload) => {
            write.lock().await.send(Message::Pong(payload)).await?;
        }
        Message::Close(_) => return Ok(false),
        _ => {}
    }
    Ok(true)
}

async fn handle_logs_subscribe(write: &SharedWsWrite, action: &str) -> Result<()> {
    let action = action.trim().to_ascii_lowercase();
    if action == "unsubscribe" || action == "stop" || action == "off" {
        crate::cloud_log::set_streaming(false);
        info!("cloud log streaming stopped");
        return Ok(());
    }
    crate::cloud_log::set_streaming(true);
    info!("cloud log streaming started");
    let lines: Vec<LogsLinePayload> = crate::cloud_log::snapshot()
        .into_iter()
        .map(|l| LogsLinePayload {
            ts_ms: l.ts_ms,
            level: l.level,
            msg: l.msg,
        })
        .collect();
    let batch = LogsBatchMessage {
        kind: "logs_batch",
        mode: "snapshot",
        lines,
    };
    let _ = ws_send_text(write, &serde_json::to_string(&batch)?).await;
    Ok(())
}

async fn flush_live_logs(write: &SharedWsWrite) -> Result<()> {
    let pending = crate::cloud_log::drain_pending();
    if pending.is_empty() {
        return Ok(());
    }
    let lines: Vec<LogsLinePayload> = pending
        .into_iter()
        .map(|l| LogsLinePayload {
            ts_ms: l.ts_ms,
            level: l.level,
            msg: l.msg,
        })
        .collect();
    let batch = LogsBatchMessage {
        kind: "logs_batch",
        mode: "live",
        lines,
    };
    let _ = ws_send_text(write, &serde_json::to_string(&batch)?).await;
    Ok(())
}

async fn handle_remote_control(write: &SharedWsWrite, action: &str) -> Result<()> {
    let action = action.trim().to_ascii_lowercase();
    match action.as_str() {
        "restart" => {
            let ack = ControlAckMessage {
                kind: "control_ack",
                action: "restart".to_string(),
                ok: true,
                detail: Some("Restarting agent…".to_string()),
            };
            let _ = ws_send_text(write, &serde_json::to_string(&ack)?).await;
            info!("remote control: restart requested");
            // Detach so the WS ack can flush before this process is replaced.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                match tokio::task::spawn_blocking(|| crate::service::restart_runtime_from_saved_token())
                    .await
                {
                    Ok(Ok(())) => {
                        // Process should be going down; exit as a fallback.
                        std::process::exit(0);
                    }
                    Ok(Err(err)) => {
                        warn!("remote restart failed: {err:#}");
                    }
                    Err(err) => {
                        warn!("remote restart task failed: {err:#}");
                    }
                }
            });
        }
        "update" => {
            let ack = ControlAckMessage {
                kind: "control_ack",
                action: "update".to_string(),
                ok: true,
                detail: Some("Checking for agent update…".to_string()),
            };
            let _ = ws_send_text(write, &serde_json::to_string(&ack)?).await;
            info!("remote control: update requested");
            let write = write.clone();
            tokio::spawn(async move {
                let result = async {
                    let outcome = crate::update::check_for_update().await?;
                    if !outcome.info().update_available {
                        anyhow::Ok((
                            false,
                            format!("Already on latest ({})", outcome.info().current_version),
                        ))
                    } else {
                        let latest = outcome.info().latest_version.clone();
                        crate::update::install_latest_update().await?;
                        anyhow::Ok((true, format!("Updated to {latest}; restarting…")))
                    }
                }
                .await;
                match result {
                    Ok((will_restart, detail)) => {
                        let ack = ControlAckMessage {
                            kind: "control_ack",
                            action: "update".to_string(),
                            ok: true,
                            detail: Some(detail),
                        };
                        let _ = ws_send_text(&write, &serde_json::to_string(&ack).unwrap_or_default()).await;
                        if will_restart {
                            tokio::time::sleep(Duration::from_millis(800)).await;
                            // Linux: binary already replaced in place; restart the unit
                            // (or exit and let Restart=always respawn the new image).
                            match tokio::task::spawn_blocking(|| {
                                crate::service::restart_runtime_from_saved_token()
                            })
                            .await
                            {
                                Ok(Ok(())) => {}
                                Ok(Err(err)) => {
                                    warn!("post-update restart failed (exiting anyway): {err:#}");
                                }
                                Err(err) => {
                                    warn!("post-update restart task failed (exiting anyway): {err:#}");
                                }
                            }
                            std::process::exit(0);
                        }
                    }
                    Err(err) => {
                        warn!("remote update failed: {err:#}");
                        let ack = ControlAckMessage {
                            kind: "control_ack",
                            action: "update".to_string(),
                            ok: false,
                            detail: Some(format!("{err:#}").chars().take(240).collect()),
                        };
                        let _ = ws_send_text(&write, &serde_json::to_string(&ack).unwrap_or_default()).await;
                    }
                }
            });
        }
        other => {
            let ack = ControlAckMessage {
                kind: "control_ack",
                action: other.to_string(),
                ok: false,
                detail: Some("Unknown control action".to_string()),
            };
            let _ = ws_send_text(write, &serde_json::to_string(&ack)?).await;
        }
    }
    Ok(())
}

async fn respond_invoke(
    state: &Arc<Mutex<SessionState>>,
    write: &SharedWsWrite,
    invoke: crate::protocol::InvokeMessage,
) -> Result<()> {
    info!(
        "invoke {} · model {} · runtime {} · stream={}",
        invoke.id, invoke.model_id, invoke.runtime_model, invoke.stream
    );
    // Push the invoke line to live cloud logs immediately (don't wait for the 1s ticker).
    let _ = flush_live_logs(write).await;

    let (hv, catalog_model, ram_gb, headroom, max_tokens) = {
        let mut guard = state.lock().await;
        let max_jobs = guard.cached_max_jobs.max(1);
        if guard.active_job_count >= max_jobs {
            // Defense in depth: router claim can free while we still run stragglers.
            let msg = InvokeErrorMessage {
                kind: "invoke_error",
                id: invoke.id.clone(),
                error: "agent_busy".to_string(),
                detail: Some("agent_busy: max concurrent jobs reached".to_string()),
            };
            let _ = ws_send_text(write, &serde_json::to_string(&msg)?).await;
            return Ok(());
        }
        guard.active_job_count = guard.active_job_count.saturating_add(1);
        guard.job_state = JobState::Busy;
        guard.active_job_id = Some(invoke.id.clone());
        guard.active_model_id = Some(invoke.model_id.clone());
        if guard.cached_idle_slots > 0 {
            guard.cached_idle_slots = guard.cached_idle_slots.saturating_sub(1);
        }
        guard.vram_lifecycle.on_job_started();
        let hv = guard
            .hypervisor
            .clone()
            .context("compute hypervisor not started")?;
        let catalog_model = guard
            .catalog
            .iter()
            .find(|m| m.model_id == invoke.model_id)
            .cloned()
            .unwrap_or(CatalogModel {
                model_id: invoke.model_id.clone(),
                display_name: invoke.model_id.clone(),
                runtime_model: invoke.runtime_model.clone(),
                max_context_tokens: 4096,
                regions: vec![],
                weight_size_gb: None,
                min_vram_gb: None,
                min_ram_gb: None,
                weights: None,
            });
        let specs = guard.enabled_devices();
        let ram_gb = specs.ram_gb.or(detect_ram_gb()).unwrap_or(0);
        let headroom = guard.cpu_ram_headroom_gb;
        let max_tokens = guard.effective_max_tokens(invoke.max_tokens);
        (hv, catalog_model, ram_gb, headroom, max_tokens)
    };
    // Do not heartbeat per-invoke — under concurrency that floods the WS with
    // heartbeat/pong/policy traffic and resets the provider connection.

    let invoke_id = invoke.id.clone();
    let runtime_model = invoke.runtime_model.clone();
    let stream = invoke.stream;

    let result = async {
        let write_delta = write.clone();
        let invoke_id_cb = invoke_id.clone();
        let on_delta: Option<Box<dyn FnMut(String) + Send>> = Some(Box::new(move |delta: String| {
            let write = write_delta.clone();
            let invoke_id = invoke_id_cb.clone();
            let forward_tokens = stream;
            tokio::spawn(async move {
                let text = if let Some(rest) = delta.strip_prefix('\u{1e}') {
                    let mut parts = rest.split('\u{1e}');
                    let phase = parts.next().unwrap_or("working").to_string();
                    let pct = parts
                        .next()
                        .and_then(|s| s.parse::<f32>().ok())
                        .filter(|v| *v >= 0.0);
                    serde_json::to_string(&crate::protocol::InvokeProgressMessage {
                        kind: "invoke_progress",
                        id: invoke_id,
                        phase,
                        pct,
                    })
                } else if delta.is_empty() || !forward_tokens {
                    serde_json::to_string(&crate::protocol::InvokeProgressMessage {
                        kind: "invoke_progress",
                        id: invoke_id,
                        phase: if delta.is_empty() {
                            "working".into()
                        } else {
                            "decode".into()
                        },
                        pct: None,
                    })
                } else {
                    serde_json::to_string(&InvokeDeltaMessage {
                        kind: "invoke_delta",
                        id: invoke_id,
                        delta,
                    })
                };
                if let Ok(text) = text {
                    let _ = write.lock().await.send(Message::Text(text)).await;
                }
            });
        }));

        match hv
            .invoke(
                &invoke.id,
                &invoke.model_id,
                &runtime_model,
                &invoke.messages,
                max_tokens,
                &catalog_model,
                ram_gb,
                headroom,
                on_delta,
            )
            .await
        {
            Ok((content, prompt_tokens, completion_tokens, timings, slot_id)) => {
                info!(slot = %slot_id, "invoke {} completed", invoke_id);
                let result = InvokeResultMessage {
                    kind: "invoke_result",
                    id: invoke_id.clone(),
                    content,
                    prompt_tokens,
                    completion_tokens,
                    timings: Some(timings),
                };
                ws_send_text(write, &serde_json::to_string(&result)?).await
            }
            Err(err) => {
                let code = invoke_error_code(&err);
                if code == "agent_busy" {
                    debug!("inference invoke busy: {err:#}");
                } else if code == "request_canceled" {
                    info!("inference invoke canceled: {err:#}");
                } else {
                    warn!("inference invoke failed: {err:#}");
                    state::record_inference_failure(code, &format!("{err:#}"));
                }
                if code == "model_load_failed" {
                    handle_weight_load_failure(&runtime_model, &err);
                }
                let msg = InvokeErrorMessage {
                    kind: "invoke_error",
                    id: invoke_id.clone(),
                    error: code.to_string(),
                    detail: Some(crate::protocol::cloud_invoke_error_detail(&err)),
                };
                // Capacity rejects must not tear down the session if the socket is racing.
                let _ = ws_send_text(write, &serde_json::to_string(&msg)?).await;
                Ok(())
            }
        }
    }
    .await;

    {
        let mut guard = state.lock().await;
        guard.active_job_count = guard.active_job_count.saturating_sub(1);
        if guard.active_job_count == 0 {
            guard.job_state = JobState::Idle;
            guard.active_job_id = None;
            guard.active_model_id = None;
            guard.vram_lifecycle.on_job_finished();
        }
    }
    result
}

async fn respond_invoke_split(
    state: &Arc<Mutex<SessionState>>,
    write: &SharedWsWrite,
    invoke: crate::protocol::InvokeSplitMessage,
) -> Result<()> {
    // Split inference stays in-process on the best idle single slot via a temporary engine.
    // Cross-node KV handoff is unchanged; local multi-GPU split uses one worker card.
    info!(
        "invoke_split {} · segment {} · model {}",
        invoke.id, invoke.segment, invoke.model_id
    );

    {
        let mut guard = state.lock().await;
        guard.active_job_count = guard.active_job_count.saturating_add(1);
        guard.job_state = JobState::Busy;
        guard.active_job_id = Some(invoke.id.clone());
        guard.active_model_id = Some(invoke.model_id.clone());
        if guard.cached_idle_slots > 0 {
            guard.cached_idle_slots = guard.cached_idle_slots.saturating_sub(1);
        }
        guard.vram_lifecycle.on_job_started();
    }

    let specs = state.lock().await.enabled_devices();
    let engine = InferenceEngine::new(&specs.compute_devices)
        .context("no enabled compute devices for split inference")?;

    let segment = invoke.segment.to_lowercase();
    let result = async {
        match segment.as_str() {
            "lower" => match engine
                .invoke_split_lower(&invoke.runtime_model, &invoke.prompt_token_ids)
                .await
            {
                Ok(output) => {
                    let result = crate::protocol::InvokeSplitResultMessage {
                        kind: "invoke_split_result",
                        id: invoke.id,
                        state_b64: output.state_b64,
                        content: String::new(),
                        prompt_tokens: output.prompt_tokens,
                        completion_tokens: 0,
                    };
                    write
                        .lock()
                        .await
                        .send(Message::Text(serde_json::to_string(&result)?))
                        .await?;
                    Ok(())
                }
                Err(err) => send_invoke_split_error(write, &invoke.id, &engine, err).await,
            },
            "upper" => {
                let max_tokens = {
                    let guard = state.lock().await;
                    guard.effective_max_tokens(invoke.max_tokens)
                };
                match engine
                    .invoke_split_upper(&invoke.runtime_model, &invoke.state_b64, max_tokens)
                    .await
                {
                    Ok(output) => {
                        let result = crate::protocol::InvokeSplitResultMessage {
                            kind: "invoke_split_result",
                            id: invoke.id,
                            state_b64: String::new(),
                            content: output.content,
                            prompt_tokens: output.prompt_tokens,
                            completion_tokens: output.completion_tokens,
                        };
                        write
                            .lock()
                            .await
                            .send(Message::Text(serde_json::to_string(&result)?))
                            .await?;
                        Ok(())
                    }
                    Err(err) => send_invoke_split_error(write, &invoke.id, &engine, err).await,
                }
            }
            "warm" => match engine.invoke_split_warm(&invoke.runtime_model).await {
                Ok(()) => {
                    let result = crate::protocol::InvokeSplitResultMessage {
                        kind: "invoke_split_result",
                        id: invoke.id,
                        state_b64: String::new(),
                        content: String::new(),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                    };
                    write
                        .lock()
                        .await
                        .send(Message::Text(serde_json::to_string(&result)?))
                        .await?;
                    Ok(())
                }
                Err(err) => send_invoke_split_error(write, &invoke.id, &engine, err).await,
            },
            other => {
                let err = InvokeErrorMessage {
                    kind: "invoke_error",
                    id: invoke.id,
                    error: "inference_failed".to_string(),
                    detail: Some(format!("unknown split segment: {other}")),
                };
                write
                    .lock()
                    .await
                    .send(Message::Text(serde_json::to_string(&err)?))
                    .await?;
                Ok(())
            }
        }
    }
    .await;

    {
        let mut guard = state.lock().await;
        guard.active_job_count = guard.active_job_count.saturating_sub(1);
        if guard.active_job_count == 0 {
            guard.job_state = JobState::Idle;
            guard.active_job_id = None;
            guard.active_model_id = None;
            guard.vram_lifecycle.on_job_finished();
        }
    }
    result
}

async fn send_invoke_split_error(
    write: &SharedWsWrite,
    id: &str,
    engine: &InferenceEngine,
    err: anyhow::Error,
) -> Result<()> {
    warn!(
        "split inference failed on pool {}: {err:#}",
        engine.pool().display_name
    );
    let code = invoke_error_code(&err);
    if code != "agent_busy" {
        state::record_inference_failure(code, &format!("{err:#}"));
    }
    let err = InvokeErrorMessage {
        kind: "invoke_error",
        id: id.to_string(),
        error: code.to_string(),
        detail: Some(crate::protocol::cloud_invoke_error_detail(&err)),
    };
    write
        .lock()
        .await
        .send(Message::Text(serde_json::to_string(&err)?))
        .await?;
    Ok(())
}

fn invoke_error_code(err: &anyhow::Error) -> &'static str {
    let detail = format!("{err:#}").to_lowercase();
    // Capacity / contention — router must not damage the machine.
    if detail.contains("request_canceled")
        || detail.contains("request_cancelled")
        || detail.contains("invoke_timeout")
    {
        "request_canceled"
    // Weight load failures must win over bare "null result from llama cpp".
    // Unsupported arch / bad GGUF often surfaces as: load model <path>: null result…
    // Misclassifying that as agent_busy makes debug UI say "Agent is busy" for a
    // model that simply cannot load on this agent build.
    } else if detail.contains("load model")
        || detail.contains("load_from_file")
        || detail.contains("weights not found")
        || detail.contains("model weights not found")
        || detail.contains("unknown model architecture")
        || detail.contains("unknown architecture")
        || (detail.contains("gguf") && detail.contains("not found"))
    {
        "model_load_failed"
    } else if detail.contains("agent_busy")
        || detail.contains("no idle compute slot")
        || detail.contains("not available")
        || (detail.contains("sibling slot") && detail.contains("busy"))
        // Backend memory fights / failed load under fanout (llama null ptr).
        || detail.contains("erroroutdevicememory")
        || detail.contains("out of device memory")
        || detail.contains("create llama context")
        || detail.contains("null reference")
        || detail.contains("null result")
    {
        "agent_busy"
    } else if detail.contains("out of memory")
        || detail.contains("oom")
        || detail.contains("cudamalloc")
        || detail.contains("failed to allocate")
        || detail.contains("no compute devices")
        || detail.contains("cuda error")
        || detail.contains("invalid device")
        || detail.contains("ggml_backend_cuda")
    {
        "model_out_of_memory"
    } else if detail.contains("context window") || detail.contains("too long") {
        "prompt_too_long"
    } else {
        "inference_failed"
    }
}

#[cfg(test)]
mod invoke_error_code_tests {
    use super::invoke_error_code;

    #[test]
    fn load_model_null_result_is_model_load_failed_not_busy() {
        let err = anyhow::anyhow!(
            "load model C:\\Users\\x\\.cache\\scalattice\\models\\Qwen__Qwen3.5-9B\\Qwen_Qwen3.5-9B-Q4_K_M.gguf: null result from llama cpp"
        );
        assert_eq!(invoke_error_code(&err), "model_load_failed");
    }

    #[test]
    fn bare_null_result_still_agent_busy() {
        let err = anyhow::anyhow!("null result from llama cpp");
        assert_eq!(invoke_error_code(&err), "agent_busy");
    }

    #[test]
    fn unknown_architecture_is_model_load_failed() {
        let err = anyhow::anyhow!("unknown model architecture: 'qwen35'");
        assert_eq!(invoke_error_code(&err), "model_load_failed");
    }
}
