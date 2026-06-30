use crate::config::{AgentConfig, SCALATTICE_WS_URL};
use crate::protocol::{
    parse_envelope, parse_error, parse_invoke, parse_invoke_split, parse_pong, parse_ready, parse_registered,
    AgentSchedule, CatalogModel, ComputeDevicePolicy, HeartbeatMessage, InvokeErrorMessage, InvokeResultMessage,
    ModelPolicyEntry, RegisterMessage,
};
use crate::vram_lifecycle::{ScheduleTransition, VramLifecycleConfig, VramLifecycleState, VramTickAction};
use crate::compute_pool::build_virtual_card;
use crate::inference::{InferenceEngine, InferenceRequest};
use crate::models::{can_host_model, model_weights_ready, purge_incomplete_model_weights, purge_model_weights, spawn_catalog_sync};
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::interval;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{info, warn};

type WsWrite = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

struct SessionState {
    registered: bool,
    compute_policy: Vec<(String, bool)>,
    model_policy: Vec<(String, bool)>,
    job_state: JobState,
    active_job_id: Option<String>,
    active_model_id: Option<String>,
    advertised_models: Vec<String>,
    node_id: Option<String>,
    catalog: Vec<CatalogModel>,
    hf_token: Option<String>,
    last_sync_token: Option<String>,
    download_cancel: Arc<AtomicBool>,
    sync_in_flight: Arc<AtomicBool>,
    logged_download_blockers: bool,
    vram_lifecycle: VramLifecycleState,
}

impl SessionState {
    fn new() -> Self {
        Self {
            registered: false,
            compute_policy: Vec::new(),
            model_policy: Vec::new(),
            job_state: JobState::Idle,
            active_job_id: None,
            active_model_id: None,
            advertised_models: Vec::new(),
            node_id: None,
            catalog: Vec::new(),
            hf_token: None,
            last_sync_token: None,
            download_cancel: Arc::new(AtomicBool::new(false)),
            sync_in_flight: Arc::new(AtomicBool::new(false)),
            logged_download_blockers: false,
            vram_lifecycle: VramLifecycleState::default(),
        }
    }

    fn vram_config(&self) -> VramLifecycleConfig {
        VramLifecycleConfig::from_env()
    }

    fn evict_vram_cache(&self) {
        info!("evicting in-memory model weights from VRAM");
        crate::llm::evict_all();
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
        self.register_model_ids()
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
                    Some(runtime)
                })
            })
            .collect()
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
            .filter(|model| {
                let runtime_model = if model.runtime_model.trim().is_empty() {
                    model.model_id.as_str()
                } else {
                    model.runtime_model.as_str()
                };
                !model_weights_ready(runtime_model)
            })
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
        let card = match build_virtual_card(&specs.compute_devices) {
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
        let card = match build_virtual_card(&specs.compute_devices) {
            Ok(card) => card,
            Err(err) => {
                warn!("model downloads blocked: {err:#}");
                return;
            }
        };
        for model in enabled {
            let runtime_model = if model.runtime_model.trim().is_empty() {
                model.model_id.as_str()
            } else {
                model.runtime_model.as_str()
            };
            if model_weights_ready(runtime_model) {
                continue;
            }
            if !can_host_model(model, &card, ram_gb) {
                warn!(
                    "model {} cannot run on this machine (needs {} GB VRAM / {} GB RAM; virtual card has {} GB VRAM, {} GB RAM)",
                    model.model_id,
                    model.min_vram_gb.unwrap_or(0),
                    model.min_ram_gb.unwrap_or(0),
                    card.total_vram_gb,
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
            .unwrap_or_else(|| model_id.to_string())
    }

    fn apply_purge_models(&mut self, model_ids: &[String]) {
        if model_ids.is_empty() {
            return;
        }
        if let Some(downloading) = crate::state::downloading_model() {
            if model_ids.iter().any(|id| id == &downloading) {
                self.cancel_active_downloads();
            }
        }
        for model_id in model_ids {
            let runtime_model = self.runtime_for_model_id(model_id);
            info!("purging model weights for {model_id} ({runtime_model})");
            purge_model_weights(&runtime_model);
        }
        self.evict_vram_cache();
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
        let card = match build_virtual_card(&specs.compute_devices) {
            Ok(card) => card,
            Err(_) => return Vec::new(),
        };

        self.catalog
            .iter()
            .filter(|model| self.is_model_enabled(&model.model_id))
            .filter(|model| can_host_model(model, &card, ram_gb))
            .cloned()
            .collect()
    }

    fn register_model_ids(&self) -> Vec<String> {
        self.eligible_catalog_models()
            .iter()
            .filter(|model| {
                let runtime_model = if model.runtime_model.trim().is_empty() {
                    model.model_id.as_str()
                } else {
                    model.runtime_model.as_str()
                };
                model_weights_ready(runtime_model)
            })
            .map(|m| m.model_id.clone())
            .collect()
    }

    fn apply_compute_devices(&mut self, devices: &[ComputeDevicePolicy]) {
        if devices.is_empty() {
            return;
        }
        self.compute_policy = devices
            .iter()
            .map(|device| (device.id.clone(), device.enabled))
            .collect();
    }

    fn enabled_devices(&self) -> crate::specs::MachineSpecs {
        let mut devices = detect_all_compute_devices();
        apply_compute_policy(&mut devices, &self.compute_policy);
        build_specs_from_devices(
            &devices,
            detect_hostname(),
            detect_cpu_model(),
            detect_ram_gb(),
            detect_driver_version(),
            detect_cuda_version(),
        )
    }

    fn live_specs(&self) -> MachineSpecs {
        let mut specs = self.enabled_devices();
        if let Ok(engine) = InferenceEngine::new(&specs.compute_devices) {
            if engine.pool().devices.len() > 1 {
                specs.gpu_name = Some(engine.pool().display_name.clone());
                specs.vram_gb = Some(engine.pool().total_vram_gb);
            }
        }
        specs
    }

    fn refresh_inference(&self) -> Result<InferenceEngine> {
        let specs = self.enabled_devices();
        let engine = InferenceEngine::new(&specs.compute_devices)?;
        if engine.pool().devices.len() > 1 {
            info!(
                "virtual compute card: {} · {:?}",
                engine.pool().display_name,
                engine.pool().strategy
            );
        }
        Ok(engine)
    }

    fn count_blocked_enabled_models(&self) -> usize {
        let specs = self.enabled_devices();
        let ram_gb = specs.ram_gb.or(detect_ram_gb()).unwrap_or(0);
        let card = match build_virtual_card(&specs.compute_devices) {
            Ok(card) => card,
            Err(_) => return 0,
        };

        self.catalog
            .iter()
            .filter(|model| self.is_model_enabled(&model.model_id))
            .filter(|model| model.weights.is_some())
            .filter(|model| {
                let runtime_model = if model.runtime_model.trim().is_empty() {
                    model.model_id.as_str()
                } else {
                    model.runtime_model.as_str()
                };
                if model_weights_ready(runtime_model) {
                    return false;
                }
                !can_host_model(model, &card, ram_gb)
            })
            .count()
    }

    fn runtime(&self) -> crate::runtime::AgentRuntime {
        let specs = self.enabled_devices();
        let loaded_models = InferenceEngine::new(&specs.compute_devices)
            .map(|engine| engine.loaded_models())
            .unwrap_or_default();
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
            crate::models::models_cache_disk_gb(),
            crate::runtime::serialize_model_disk(&crate::models::list_model_disk_status()),
        )
    }

    fn persist_local_state(&self) {
        let runtime = self.runtime();
        let devices = self.enabled_devices().compute_devices;
        state::update_connection_state(
            Some(runtime.status_label),
            self.node_id.clone(),
            true,
            self.registered,
            None,
            devices,
        );
    }
}

pub async fn run_agent(config: AgentConfig) -> Result<()> {
    if let Err(err) = crate::llm::init_backend() {
        warn!("embedded llama.cpp backend init failed: {err:#}");
    }

    let specs = detect_machine_specs();
    info!("{}", crate::specs::status_line(&specs));

    let mut backoff = Duration::from_secs(3);
    loop {
        match run_agent_session(&config).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                state::mark_disconnected(Some(format!("{err:#}")));
                warn!("disconnected: {err:#}; reconnecting in {:?}...", backoff);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(120));
            }
        }
    }
}

async fn run_agent_session(config: &AgentConfig) -> Result<()> {
    let state = Arc::new(Mutex::new(SessionState::new()));

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

    let (mut write, mut read) = ws.split();
    let mut heartbeat = interval(Duration::from_secs(25));

    loop {
        tokio::select! {
            msg = read.next() => {
                let Some(msg) = msg else {
                    bail!("connection closed by server");
                };
                let msg = msg.context("websocket read error")?;
                if !handle_server_message(config, &state, &mut write, msg).await? {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                let registered = {
                    let mut guard = state.lock().await;
                    guard.tick_vram_lifecycle();
                    if guard.registered {
                        let hf_token = guard.hf_token.clone();
                        guard.sync_model_weights(hf_token, &config.token);
                    }
                    guard.registered
                };
                if registered {
                    maybe_warm_models(state.clone()).await;
                    let reregister = state.lock().await.needs_reregister();
                    if reregister {
                        send_register_message(&state, &mut write).await?;
                    } else {
                        send_heartbeat(&state, &mut write).await?;
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

async fn maybe_warm_models(state: Arc<Mutex<SessionState>>) {
    let (should_preload, warm_models, pool) = {
        let guard = state.lock().await;
        let config = guard.vram_config();
        if !guard.vram_lifecycle.should_preload(&config) {
            return;
        }
        let warm_models = guard.warm_runtime_models();
        if warm_models.is_empty() {
            return;
        }
        let Ok(engine) = guard.refresh_inference() else {
            return;
        };
        (true, warm_models, engine.pool().clone())
    };
    if !should_preload {
        return;
    }
    let state_for_task = state.clone();
    tokio::spawn(async move {
        if crate::inference::warm_cached_models(&pool, &warm_models)
            .await
            .is_ok()
        {
            let mut guard = state_for_task.lock().await;
            guard.vram_lifecycle.on_vram_loaded();
        }
    });
}

async fn send_register_message(state: &Arc<Mutex<SessionState>>, write: &mut WsWrite) -> Result<()> {
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
        .send(Message::Text(serde_json::to_string(&register)?))
        .await?;
    Ok(())
}

async fn send_heartbeat(state: &Arc<Mutex<SessionState>>, write: &mut WsWrite) -> Result<()> {
    let (specs, runtime) = {
        let guard = state.lock().await;
        (guard.live_specs(), guard.runtime())
    };
    let hb = serde_json::to_string(&HeartbeatMessage {
        kind: "heartbeat",
        specs: Some(specs),
        runtime: Some(runtime),
    })?;
    write.send(Message::Text(hb)).await?;
    state::touch_connection_state();
    Ok(())
}

async fn handle_server_message(
    config: &AgentConfig,
    state: &Arc<Mutex<SessionState>>,
    write: &mut WsWrite,
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
                    send_register_message(state, write).await?;
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
                    respond_invoke(state, write, invoke).await?;
                }
                "invoke_split" => {
                    let invoke = parse_invoke_split(data)?;
                    respond_invoke_split(state, write, invoke).await?;
                }
                "pong" => {
                    if let Ok(pong) = parse_pong(data) {
                        let transition = {
                            let mut guard = state.lock().await;
                            guard.apply_compute_devices(&pong.compute_devices);
                            guard.apply_model_policy(&pong.enabled_models);
                            guard.apply_purge_models(&pong.purge_models);
                            let transition = guard.apply_schedule(pong.schedule.clone());
                            guard.sync_model_weights(pong.hugging_face_token.clone(), &config.token);
                            guard.tick_vram_lifecycle();
                            guard.persist_local_state();
                            transition
                        };
                        if transition.entered_earning {
                            maybe_warm_models(state.clone()).await;
                        }
                    }
                }
                "error" => {
                    let err = parse_error(data)?;
                    let message = err
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    return Err(anyhow!("server error: {message}"));
                }
                other => {
                    info!("ignored message type: {other}");
                }
            }
        }
        Message::Ping(payload) => {
            write.send(Message::Pong(payload)).await?;
        }
        Message::Close(_) => return Ok(false),
        _ => {}
    }
    Ok(true)
}

async fn respond_invoke(
    state: &Arc<Mutex<SessionState>>,
    write: &mut WsWrite,
    invoke: crate::protocol::InvokeMessage,
) -> Result<()> {
    info!(
        "invoke {} · model {} · runtime {}",
        invoke.id, invoke.model_id, invoke.runtime_model
    );

    {
        let mut guard = state.lock().await;
        guard.job_state = JobState::Busy;
        guard.active_job_id = Some(invoke.id.clone());
        guard.active_model_id = Some(invoke.model_id.clone());
        guard.vram_lifecycle.on_job_started();
    }
    send_heartbeat(state, write).await?;

    let engine = {
        let guard = state.lock().await;
        guard
            .refresh_inference()
            .context("no enabled compute devices for inference")?
    };

    let result = async {
        match engine
            .invoke(InferenceRequest {
                job_id: &invoke.id,
                model_id: &invoke.model_id,
                runtime_model: &invoke.runtime_model,
                messages: &invoke.messages,
            })
            .await
        {
            Ok(output) => {
                let result = InvokeResultMessage {
                    kind: "invoke_result",
                    id: invoke.id,
                    content: output.content,
                    prompt_tokens: output.prompt_tokens,
                    completion_tokens: output.completion_tokens,
                };
                write
                    .send(Message::Text(serde_json::to_string(&result)?))
                    .await?;
                Ok(())
            }
            Err(err) => {
                let err = InvokeErrorMessage {
                    kind: "invoke_error",
                    id: invoke.id,
                    error: format!(
                        "Virtual card {}: {err:#}",
                        engine.pool().display_name
                    ),
                };
                write
                    .send(Message::Text(serde_json::to_string(&err)?))
                    .await?;
                Ok(())
            }
        }
    }
    .await;

    {
        let mut guard = state.lock().await;
        guard.job_state = JobState::Idle;
        guard.active_job_id = None;
        guard.active_model_id = None;
        guard.vram_lifecycle.on_job_finished();
    }
    send_heartbeat(state, write).await?;

    result
}

async fn respond_invoke_split(
    state: &Arc<Mutex<SessionState>>,
    write: &mut WsWrite,
    invoke: crate::protocol::InvokeSplitMessage,
) -> Result<()> {
    info!(
        "invoke_split {} · segment {} · model {}",
        invoke.id, invoke.segment, invoke.model_id
    );

    {
        let mut guard = state.lock().await;
        guard.job_state = JobState::Busy;
        guard.active_job_id = Some(invoke.id.clone());
        guard.active_model_id = Some(invoke.model_id.clone());
        guard.vram_lifecycle.on_job_started();
    }
    send_heartbeat(state, write).await?;

    let engine = {
        let guard = state.lock().await;
        guard
            .refresh_inference()
            .context("no enabled compute devices for inference")?
    };

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
                        .send(Message::Text(serde_json::to_string(&result)?))
                        .await?;
                    Ok(())
                }
                Err(err) => send_invoke_split_error(write, &invoke.id, &engine, err).await,
            },
            "upper" => match engine
                .invoke_split_upper(
                    &invoke.runtime_model,
                    &invoke.state_b64,
                    invoke.max_tokens.max(1),
                )
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
                    error: format!("unknown split segment: {other}"),
                };
                write
                    .send(Message::Text(serde_json::to_string(&err)?))
                    .await?;
                Ok(())
            }
        }
    }
    .await;

    {
        let mut guard = state.lock().await;
        guard.job_state = JobState::Idle;
        guard.active_job_id = None;
        guard.active_model_id = None;
        guard.vram_lifecycle.on_job_finished();
    }
    send_heartbeat(state, write).await?;

    result
}

async fn send_invoke_split_error(
    write: &mut WsWrite,
    id: &str,
    engine: &InferenceEngine,
    err: anyhow::Error,
) -> Result<()> {
    let err = InvokeErrorMessage {
        kind: "invoke_error",
        id: id.to_string(),
        error: format!("Virtual card {}: {err:#}", engine.pool().display_name),
    };
    write
        .send(Message::Text(serde_json::to_string(&err)?))
        .await?;
    Ok(())
}
