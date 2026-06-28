use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::specs::ComputeDevice;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLocalState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloading_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default)]
    pub server_connected: bool,
    #[serde(default)]
    pub server_registered: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compute_devices: Vec<ComputeDevice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub updated_at_ms: u64,
}

pub fn state_file_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".config/scalattice/agent.state.json"))
}

pub fn update_connection_state(
    status_label: Option<String>,
    node_id: Option<String>,
    server_connected: bool,
    server_registered: bool,
    last_error: Option<String>,
    compute_devices: Vec<ComputeDevice>,
) {
    let Some(path) = state_file_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = fs::create_dir_all(parent);

    let mut state = read_state().unwrap_or(AgentLocalState {
        status_label: None,
        downloading_model: None,
        node_id: None,
        server_connected: false,
        server_registered: false,
        compute_devices: Vec::new(),
        last_error: None,
        updated_at_ms: 0,
    });
    if let Some(label) = status_label {
        if state.downloading_model.is_none() {
            state.status_label = Some(label);
        }
    }
    if let Some(id) = node_id {
        state.node_id = Some(id);
    }
    state.server_connected = server_connected;
    state.server_registered = server_registered;
    if !compute_devices.is_empty() {
        state.compute_devices = compute_devices;
    }
    if let Some(err) = last_error {
        state.last_error = Some(err);
    } else if server_registered {
        state.last_error = None;
    }
    state.updated_at_ms = now_ms();

    write_state(&state);
}

pub fn touch_connection_state() {
    let Some(mut state) = read_state() else {
        return;
    };
    state.updated_at_ms = now_ms();
    write_state(&state);
}

pub fn mark_disconnected(error: Option<String>) {
    if state_file_path().is_none() {
        return;
    }
    let mut state = read_state().unwrap_or(AgentLocalState {
        status_label: None,
        downloading_model: None,
        node_id: None,
        server_connected: false,
        server_registered: false,
        compute_devices: Vec::new(),
        last_error: None,
        updated_at_ms: 0,
    });
    state.server_connected = false;
    state.server_registered = false;
    state.status_label = None;
    state.downloading_model = None;
    if let Some(err) = error {
        state.last_error = Some(err);
    }
    state.updated_at_ms = now_ms();
    write_state(&state);
}

pub fn set_downloading_model(model_id: Option<&str>) {
    let Some(path) = state_file_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = fs::create_dir_all(parent);

    let mut state = read_state().unwrap_or(AgentLocalState {
        status_label: None,
        downloading_model: None,
        node_id: None,
        server_connected: false,
        server_registered: false,
        compute_devices: Vec::new(),
        last_error: None,
        updated_at_ms: 0,
    });

    state.downloading_model = model_id.map(str::to_string);
    if let Some(id) = model_id {
        state.status_label = Some(format!("Downloading {id}"));
    }
    state.updated_at_ms = now_ms();
    write_state(&state);
}

pub fn downloading_model() -> Option<String> {
    read_state()
        .filter(|state| is_recent(state.updated_at_ms))
        .and_then(|state| state.downloading_model)
}

pub fn read_state() -> Option<AgentLocalState> {
    let path = state_file_path()?;
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn cloud_connection_line() -> String {
    let Some(state) = read_state() else {
        return "Scalattice Cloud: not connected".to_string();
    };

    if let Some(err) = &state.last_error {
        if !is_recent(state.updated_at_ms) {
            return format!("Scalattice Cloud: not connected ({err})");
        }
    }

    if !is_recent(state.updated_at_ms) {
        return "Scalattice Cloud: not connected".to_string();
    }

    if state.server_registered {
        return "Scalattice Cloud: connected".to_string();
    }

    if state.server_connected {
        return "Scalattice Cloud: connecting…".to_string();
    }

    "Scalattice Cloud: not connected".to_string()
}

pub struct AgentActivitySummary {
    pub status: String,
    pub node_id: Option<String>,
}

pub fn agent_activity_summary() -> Option<AgentActivitySummary> {
    let state = read_state()?;

    if let Some(err) = &state.last_error {
        if !is_recent(state.updated_at_ms) {
            return Some(AgentActivitySummary {
                status: format!("not connected ({err})"),
                node_id: None,
            });
        }
    }

    if !is_recent(state.updated_at_ms) {
        if service_hint() {
            return Some(AgentActivitySummary {
                status: "not registered (check: journalctl --user -u scalattice-agent -n 30)".to_string(),
                node_id: state.node_id,
            });
        }
        return Some(AgentActivitySummary {
            status: "not connected".to_string(),
            node_id: None,
        });
    }

    if state.server_registered {
        let status = state
            .status_label
            .as_deref()
            .map(normalize_status_label)
            .unwrap_or_else(|| "registered".to_string());
        return Some(AgentActivitySummary {
            status,
            node_id: state.node_id,
        });
    }

    if state.server_connected {
        return Some(AgentActivitySummary {
            status: "connecting".to_string(),
            node_id: state.node_id,
        });
    }

    Some(AgentActivitySummary {
        status: "not connected".to_string(),
        node_id: None,
    })
}

fn normalize_status_label(label: &str) -> String {
    let trimmed = label.trim();
    let stripped = trimmed
        .strip_prefix("Connected · ")
        .or_else(|| trimmed.strip_prefix("Connected - "))
        .unwrap_or(trimmed);
    stripped.to_string()
}

fn service_hint() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", "scalattice-agent.service"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn is_recent(updated_at_ms: u64) -> bool {
    now_ms().saturating_sub(updated_at_ms) < 120_000
}

fn write_state(state: &AgentLocalState) {
    let Some(path) = state_file_path() else {
        return;
    };
    if let Ok(raw) = serde_json::to_string_pretty(state) {
        let _ = fs::write(path, raw);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
