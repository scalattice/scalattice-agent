use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLocalState {
    pub demo_mode: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub updated_at_ms: u64,
}

pub fn state_file_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".config/scalattice/agent.state.json"))
}

pub fn update_connection_state(
    demo_mode: bool,
    status_label: Option<String>,
    node_id: Option<String>,
) {
    let Some(path) = state_file_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = fs::create_dir_all(parent);

    let mut state = read_state().unwrap_or(AgentLocalState {
        demo_mode: false,
        status_label: None,
        node_id: None,
        updated_at_ms: 0,
    });
    state.demo_mode = demo_mode;
    if let Some(label) = status_label {
        state.status_label = Some(label);
    }
    if let Some(id) = node_id {
        state.node_id = Some(id);
    }
    state.updated_at_ms = now_ms();

    if let Ok(raw) = serde_json::to_string_pretty(&state) {
        let _ = fs::write(path, raw);
    }
}

pub fn read_state() -> Option<AgentLocalState> {
    let path = state_file_path()?;
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn demo_status_line() -> String {
    let Some(state) = read_state() else {
        return "unknown (not connected yet)".to_string();
    };

    if !is_recent(state.updated_at_ms) {
        return "unknown (agent not connected - run: scalattice-agent connect)".to_string();
    }

    if state.demo_mode {
        "on (echo only)".to_string()
    } else {
        "off".to_string()
    }
}

pub fn connection_status_line() -> Option<String> {
    let state = read_state()?;
    if !is_recent(state.updated_at_ms) {
        return None;
    }
    state.status_label
}

fn is_recent(updated_at_ms: u64) -> bool {
    now_ms().saturating_sub(updated_at_ms) < 120_000
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
