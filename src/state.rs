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
    #[serde(default)]
    pub server_connected: bool,
    #[serde(default)]
    pub server_registered: bool,
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
    demo_mode: bool,
    status_label: Option<String>,
    node_id: Option<String>,
    server_connected: bool,
    server_registered: bool,
    last_error: Option<String>,
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
        server_connected: false,
        server_registered: false,
        last_error: None,
        updated_at_ms: 0,
    });
    state.demo_mode = demo_mode;
    if let Some(label) = status_label {
        state.status_label = Some(label);
    }
    if let Some(id) = node_id {
        state.node_id = Some(id);
    }
    state.server_connected = server_connected;
    state.server_registered = server_registered;
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
    let Some(path) = state_file_path() else {
        return;
    };
    let mut state = read_state().unwrap_or(AgentLocalState {
        demo_mode: false,
        status_label: None,
        node_id: None,
        server_connected: false,
        server_registered: false,
        last_error: None,
        updated_at_ms: 0,
    });
    state.server_connected = false;
    state.server_registered = false;
    state.status_label = None;
    if let Some(err) = error {
        state.last_error = Some(err);
    }
    state.updated_at_ms = now_ms();
    write_state(&state);
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

pub fn server_status_line() -> String {
    let Some(state) = read_state() else {
        return "not connected (no active agent session)".to_string();
    };

    if let Some(err) = &state.last_error {
        if !is_recent(state.updated_at_ms) {
            return format!("not connected ({err})");
        }
    }

    if !is_recent(state.updated_at_ms) {
        if service_hint() {
            return "not connected (background service running but not registered - check: journalctl --user -u scalattice-agent -n 30)".to_string();
        }
        return "not connected (run: scalattice-agent connect)".to_string();
    }

    if state.server_registered {
        let node = state
            .node_id
            .as_deref()
            .unwrap_or("unknown");
        let runtime = state
            .status_label
            .as_deref()
            .unwrap_or("registered");
        return format!("connected · {runtime} · node {node}");
    }

    if state.server_connected {
        return "connecting (waiting for server registration)".to_string();
    }

    "not connected".to_string()
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
