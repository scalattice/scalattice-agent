use anyhow::{bail, Context, Result};
use std::env;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub token: String,
    pub ws_url: String,
    pub region: String,
    pub models: Vec<String>,
}

impl AgentConfig {
    pub fn from_env_and_cli(
        token: Option<String>,
        ws_url: Option<String>,
        region: Option<String>,
        models: Option<String>,
    ) -> Result<Self> {
        let token = token
            .or_else(|| env::var("SCALATTICE_AGENT_TOKEN").ok())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .context("Set SCALATTICE_AGENT_TOKEN or pass --token")?;

        if !token.starts_with("slt_provider_") {
            bail!("Agent token must start with slt_provider_ (create one on Scalattice Cloud → Providers)");
        }

        let ws_url = ws_url
            .or_else(|| env::var("SCALATTICE_AGENT_WS").ok())
            .unwrap_or_else(|| "wss://api.scalattice.cloud/v1/operators/agent/ws".to_string());

        let region = region
            .or_else(|| env::var("SCALATTICE_AGENT_REGION").ok())
            .unwrap_or_else(|| "auto".to_string());

        let models = models
            .or_else(|| env::var("SCALATTICE_AGENT_MODELS").ok())
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            token,
            ws_url,
            region,
            models,
        })
    }
}
