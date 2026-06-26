use crate::config::AgentConfig;
use crate::protocol::{
    parse_envelope, parse_error, parse_invoke, parse_pong, parse_ready, parse_registered, CatalogModel,
    HeartbeatMessage, InvokeErrorMessage, InvokeResultMessage, RegisterMessage,
};
use crate::runtime::{build_runtime, JobState};
use crate::specs::detect_machine_specs;
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
    demo_mode: bool,
    job_state: JobState,
    active_job_id: Option<String>,
    active_model_id: Option<String>,
    advertised_models: Vec<String>,
    loaded_models: Vec<String>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            registered: false,
            demo_mode: false,
            job_state: JobState::Idle,
            active_job_id: None,
            active_model_id: None,
            advertised_models: Vec::new(),
            loaded_models: Vec::new(),
        }
    }

    fn set_demo_mode(&mut self, demo_mode: bool) {
        self.demo_mode = demo_mode;
    }

    fn runtime(&self) -> crate::runtime::AgentRuntime {
        build_runtime(
            self.demo_mode,
            self.job_state,
            self.active_job_id.clone(),
            self.active_model_id.clone(),
            &self.loaded_models,
        )
    }

    fn persist_local_state(&self, node_id: Option<&str>) {
        let runtime = self.runtime();
        state::update_connection_state(
            self.demo_mode,
            Some(runtime.status_label),
            node_id.map(str::to_string),
        );
    }
}

pub async fn run_agent(config: AgentConfig) -> Result<()> {
    let specs = detect_machine_specs();
    info!("{}", crate::specs::status_line(&specs));

    let mut backoff = Duration::from_secs(3);
    loop {
        match run_agent_session(&config).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                warn!("disconnected: {err:#}; reconnecting in {:?}…", backoff);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(120));
            }
        }
    }
}

async fn run_agent_session(config: &AgentConfig) -> Result<()> {
    let state = Arc::new(Mutex::new(SessionState::new()));

    let mut request = config
        .ws_url
        .as_str()
        .into_client_request()
        .context("invalid WebSocket URL")?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {}", config.token).parse()?);

    let (ws, _) = connect_async(request)
        .await
        .context("WebSocket connect failed (check token and network)")?;

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
        (detect_machine_specs(), guard.runtime())
    };
    let hb = serde_json::to_string(&HeartbeatMessage {
        kind: "heartbeat",
        specs: Some(specs),
        runtime: Some(runtime),
    })?;
    write.send(Message::Text(hb)).await?;
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
                        guard.set_demo_mode(ready.demo_mode);
                        guard.persist_local_state(Some(&ready.node_id));
                        if ready.demo_mode {
                            info!("demo mode enabled for this GPU (dashboard setting)");
                        }
                    }
                    let models = pick_models(&config.models, &ready.catalog);
                    let specs = detect_machine_specs();
                    let runtime = {
                        let mut guard = state.lock().await;
                        guard.advertised_models = models.clone();
                        guard.runtime()
                    };
                    let register = RegisterMessage {
                        kind: "register",
                        region: config.region.clone(),
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
                        guard.advertised_models = reg.models.clone();
                        guard.persist_local_state(Some(&reg.node_id));
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
                "pong" => {
                    if let Ok(pong) = parse_pong(data) {
                        let mut guard = state.lock().await;
                        if let Some(demo_mode) = pong.demo_mode {
                            if guard.demo_mode != demo_mode {
                                guard.set_demo_mode(demo_mode);
                                info!(
                                    "demo mode {}",
                                    if demo_mode { "enabled" } else { "disabled" }
                                );
                            }
                        }
                        guard.persist_local_state(None);
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

fn pick_models(requested: &[String], catalog: &[CatalogModel]) -> Vec<String> {
    if requested.is_empty() {
        return catalog.iter().map(|m| m.model_id.clone()).collect();
    }
    let allowed: std::collections::HashSet<_> = catalog.iter().map(|m| m.model_id.as_str()).collect();
    requested
        .iter()
        .filter(|id| allowed.contains(id.as_str()))
        .cloned()
        .collect()
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

    let demo_mode = state.lock().await.demo_mode;
    let result = async {
        if demo_mode {
            let user = invoke
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "user")
                .map(|m| m.content.as_str())
                .unwrap_or("");

            let content = format!("[demo agent echo] {user}");
            let prompt_tokens = estimate_tokens(&invoke.messages);
            let completion_tokens = estimate_tokens(&[crate::protocol::ChatMessage {
                role: "assistant".to_string(),
                content: content.clone(),
            }]);

            let result = InvokeResultMessage {
                kind: "invoke_result",
                id: invoke.id,
                content,
                prompt_tokens,
                completion_tokens,
            };
            write
                .send(Message::Text(serde_json::to_string(&result)?))
                .await?;
            Ok(())
        } else {
            let err = InvokeErrorMessage {
                kind: "invoke_error",
                id: invoke.id,
                error: format!(
                    "Model runtime not loaded on this agent yet. Pull weights for {} or enable Demo mode on this GPU in the Scalattice Cloud dashboard.",
                    invoke.runtime_model
                ),
            };
            write
                .send(Message::Text(serde_json::to_string(&err)?))
                .await?;
            Ok(())
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

fn estimate_tokens(messages: &[crate::protocol::ChatMessage]) -> u32 {
    let chars: usize = messages.iter().map(|m| m.content.len()).sum();
    ((chars / 4).max(1)) as u32
}
