use anyhow::{bail, Context, Result};
use std::env;
use std::path::PathBuf;

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
    if let Ok(token) = env::var("SCALATTICE_AGENT_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Some(token);
        }
    }

    let home = env::var("HOME").ok()?;
    for name in ["agent.env", "agent.systemd.env"] {
        let path = PathBuf::from(&home)
            .join(".config/scalattice")
            .join(name);
        let raw = std::fs::read_to_string(path).ok()?;
        if let Some(token) = parse_token_from_env_file(&raw) {
            return Some(token);
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
