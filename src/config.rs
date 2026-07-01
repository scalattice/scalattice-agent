use crate::paths::{agent_env_path, config_dir};
use anyhow::{bail, Context, Result};
use std::env;

/// Hard-coded Scalattice Cloud operator WebSocket endpoint (not configurable).
pub const SCALATTICE_WS_URL: &str = "wss://api.scalattice.cloud/v1/operators/agent/ws";

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub token: String,
}

fn parse_token_from_env_file(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let assignment = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let Some((key, value)) = assignment.split_once('=') else {
            continue;
        };
        if key.trim() != "SCALATTICE_AGENT_TOKEN" {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

pub fn read_saved_agent_token() -> Option<String> {
    if let Some(token) = read_token_from_config_files() {
        return Some(token);
    }

    if let Ok(token) = env::var("SCALATTICE_AGENT_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Some(token);
        }
    }

    None
}

fn read_token_from_config_files() -> Option<String> {
    let config = config_dir().ok()?;
    for name in ["agent.env", "agent.systemd.env"] {
        let path = config.join(name);
        let raw = std::fs::read_to_string(path).ok()?;
        if let Some(token) = parse_token_from_env_file(&raw) {
            return Some(token);
        }
    }

    if let Ok(path) = agent_env_path() {
        if let Ok(raw) = std::fs::read_to_string(path) {
            if let Some(token) = parse_token_from_env_file(&raw) {
                return Some(token);
            }
        }
    }

    None
}

impl AgentConfig {
    pub fn from_env_and_cli(token: Option<String>) -> Result<Self> {
        let token = token
            .or_else(read_saved_agent_token)
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .context(
                "Set SCALATTICE_AGENT_TOKEN, pass --token, or run: scalattice-agent set-token --token slt_provider_…",
            )?;

        if !token.starts_with("slt_provider_") {
            bail!("Agent token must start with slt_provider_ (create one on Scalattice Cloud → Providers)");
        }

        Ok(Self { token })
    }
}
