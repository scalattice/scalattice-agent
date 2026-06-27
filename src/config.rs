use anyhow::{bail, Context, Result};
use std::env;

/// Hard-coded Scalattice Cloud operator WebSocket endpoint (not configurable).
pub const SCALATTICE_WS_URL: &str = "wss://api.scalattice.cloud/v1/operators/agent/ws";

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub token: String,
}

impl AgentConfig {
    pub fn from_env_and_cli(token: Option<String>) -> Result<Self> {
        let token = token
            .or_else(|| env::var("SCALATTICE_AGENT_TOKEN").ok())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .context("Set SCALATTICE_AGENT_TOKEN or pass --token")?;

        if !token.starts_with("slt_provider_") {
            bail!("Agent token must start with slt_provider_ (create one on Scalattice Cloud → Providers)");
        }

        Ok(Self { token })
    }
}
