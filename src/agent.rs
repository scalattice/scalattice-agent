use crate::config::AgentConfig;
use crate::protocol::{
    parse_envelope, parse_error, parse_invoke, parse_ready, parse_registered, CatalogModel,
    HeartbeatMessage, InvokeErrorMessage, InvokeResultMessage, RegisterMessage,
};
use crate::specs::{detect_machine_specs, MachineSpecs};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::time::interval;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

pub async fn run_agent(config: AgentConfig) -> Result<()> {
    let specs = detect_machine_specs();
    info!("{}", crate::specs::status_line(&specs));

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
    let mut registered = false;
    let mut heartbeat = interval(Duration::from_secs(25));

    loop {
        tokio::select! {
            msg = read.next() => {
                let Some(msg) = msg else {
                    bail!("connection closed by server");
                };
                let msg = msg.context("websocket read error")?;
                if !handle_server_message(&config, &mut write, &mut registered, msg).await? {
                    break;
                }
            }
            _ = heartbeat.tick(), if registered => {
                let specs = detect_machine_specs();
                let hb = serde_json::to_string(&HeartbeatMessage {
                    kind: "heartbeat",
                    specs: Some(specs),
                })?;
                write
                    .send(Message::Text(hb))
                    .await
                    .map_err(|err| anyhow!("websocket write failed: {err}"))?;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                break;
            }
        }
    }

    Ok(())
}

async fn handle_server_message<W>(
    config: &AgentConfig,
    write: &mut W,
    registered: &mut bool,
    msg: Message,
) -> Result<bool>
where
    W: SinkExt<Message> + Unpin,
{
    match msg {
        Message::Text(text) => {
            let data = text.as_bytes();
            let env = parse_envelope(data)?;
            match env.kind.as_str() {
                "ready" => {
                    let ready = parse_ready(data)?;
                    info!("assigned node {}", ready.node_id);
                    let models = pick_models(&config.models, &ready.catalog);
                    let specs = detect_machine_specs();
                    let register = register_message(config.region.clone(), models, specs);
                    write
                        .send(Message::Text(serde_json::to_string(&register)?))
                        .await
                        .map_err(|err| anyhow!("websocket write failed: {err}"))?;
                }
                "registered" => {
                    let reg = parse_registered(data)?;
                    *registered = true;
                    info!(
                        "registered · node {} · models: {}",
                        reg.node_id,
                        reg.models.join(", ")
                    );
                    if config.demo_mode {
                        warn!("demo mode enabled (SCALATTICE_AGENT_DEMO=1): echo responses only");
                    }
                }
                "invoke" => {
                    let invoke = parse_invoke(data)?;
                    respond_invoke(config, write, invoke).await?;
                }
                "pong" => {}
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
            write
                .send(Message::Pong(payload))
                .await
                .map_err(|err| anyhow!("websocket write failed: {err}"))?;
        }
        Message::Close(_) => return Ok(false),
        _ => {}
    }
    Ok(true)
}

fn register_message(region: String, models: Vec<String>, specs: MachineSpecs) -> RegisterMessage {
    RegisterMessage {
        kind: "register",
        region,
        models,
        gpu_name: specs.gpu_name.clone(),
        vram_gb: specs.vram_gb,
        specs: Some(specs),
    }
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

async fn respond_invoke<W>(config: &AgentConfig, write: &mut W, invoke: crate::protocol::InvokeMessage) -> Result<()>
where
    W: SinkExt<Message> + Unpin,
{
    info!(
        "invoke {} · model {} · runtime {}",
        invoke.id, invoke.model_id, invoke.runtime_model
    );

    if config.demo_mode {
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
            .await
            .map_err(|err| anyhow!("websocket write failed: {err}"))?;
        return Ok(());
    }

    let err = InvokeErrorMessage {
        kind: "invoke_error",
        id: invoke.id,
        error: format!(
            "Model runtime not loaded on this agent yet. Pull weights for {} and enable SCALATTICE_AGENT_DEMO=1 for connectivity testing.",
            invoke.runtime_model
        ),
    };
    write
        .send(Message::Text(serde_json::to_string(&err)?))
        .await
        .map_err(|e| anyhow!("websocket write failed: {e}"))?;
    Ok(())
}

fn estimate_tokens(messages: &[crate::protocol::ChatMessage]) -> u32 {
    let chars: usize = messages.iter().map(|m| m.content.len()).sum();
    ((chars / 4).max(1)) as u32
}
