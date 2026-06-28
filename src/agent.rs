use crate::config::{AgentConfig, SCALATTICE_WS_URL};
use crate::protocol::{
    parse_envelope, parse_error, parse_invoke, parse_invoke_split, parse_pong, parse_ready, parse_registered,
    CatalogModel, ComputeDevicePolicy, HeartbeatMessage, InvokeErrorMessage, InvokeResultMessage, ModelPolicyEntry,
    RegisterMessage,
};
use crate::compute_pool::build_virtual_card;
use crate::inference::{InferenceEngine, InferenceRequest};
use crate::models::can_host_model;
use crate::models::spawn_catalog_sync;
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
    weights_synced: bool,
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
            weights_synced: false,
        }
    }

    fn effective_hf_token(&self, server_token: Option<String>) -> Option<String> {
        server_token
            .or_else(|| self.hf_token.clone())
            .or_else(|| std::env::var("SCALATTICE_HF_TOKEN").ok())
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
    }

    fn sync_model_weights(&mut self, server_token: Option<String>, agent_token: &str) {
        if self.weights_synced {
            return;
        }
        let eligible = self.eligible_catalog_models();
        if eligible.is_empty() {
            return;
        }
        let token = self.effective_hf_token(server_token);
        let can_mirror = eligible.iter().any(|m| {
            m.weights
                .as_ref()
                .and_then(|w| w.mirror_url.as_deref())
                .is_some_and(|url| !url.trim().is_empty())
        });
        if token.is_none() && !can_mirror && eligible.iter().any(|m| m.weights.is_some()) {
            warn!("model downloads are not configured on the server yet (contact Scalattice support)");
            return;
        }
        self.weights_synced = true;
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
            eligible.len()
        );
        spawn_catalog_sync(eligible, card, ram_gb, agent_token.to_string(), token);
    }

    fn apply_model_policy(&mut self, models: &[ModelPolicyEntry]) {
        if models.is_empty() {
            return;
        }
        let next: Vec<(String, bool)> = models
            .iter()
            .map(|model| (model.model_id.clone(), model.enabled))
            .collect();
        if self.model_policy != next {
            self.model_policy = next;
            self.weights_synced = false;
        }
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

    fn runtime(&self) -> crate::runtime::AgentRuntime {
        let specs = self.enabled_devices();
        let loaded_models = InferenceEngine::new(&specs.compute_devices)
            .map(|engine| engine.loaded_models())
            .unwrap_or_default();
        let enabled_count = specs.compute_devices.iter().filter(|d| d.enabled).count();
        let downloading = crate::state::downloading_model();

        build_runtime(
            self.job_state,
            self.active_job_id.clone(),
            self.active_model_id.clone(),
            &loaded_models,
            enabled_count,
            downloading.as_deref(),
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
                let registered = state.lock().await.registered;
                if registered {
                    send_heartbeat(&state, &mut write).await?;
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
                        guard.weights_synced = false;
                        guard.sync_model_weights(ready.hugging_face_token.clone(), &config.token);
                        guard.persist_local_state();
                    }
                    if let Ok(engine) = state.lock().await.refresh_inference() {
                        let _ = crate::inference::warm_pool_devices(engine.pool()).await;
                    }
                    let models = {
                        let guard = state.lock().await;
                        guard.register_model_ids()
                    };
                    let specs = {
                        let guard = state.lock().await;
                        guard.live_specs()
                    };
                    let runtime = {
                        let mut guard = state.lock().await;
                        guard.advertised_models = models.clone();
                        guard.runtime()
                    };
                    let register = RegisterMessage {
                        kind: "register",
                        models,
                        gpu_name: specs.gpu_name.clone(),
                        vram_gb: specs.vram_gb,
                        specs: Some(specs),
                        runtime: Some(runtime),
                    };
                    write
                        .send(Message::Text(serde_json::to_string(&register)?))
                        .await?;
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
                        let mut guard = state.lock().await;
                        guard.apply_compute_devices(&pong.compute_devices);
                        guard.apply_model_policy(&pong.enabled_models);
                        if pong.hugging_face_token.is_some() && !guard.weights_synced {
                            guard.sync_model_weights(pong.hugging_face_token.clone(), &config.token);
                        } else if !guard.weights_synced {
                            guard.sync_model_weights(None, &config.token);
                        }
                        guard.persist_local_state();
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
