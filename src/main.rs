mod agent;
mod config;
mod protocol;
mod specs;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "scalattice-agent",
    about = "Scalattice GPU operator agent (open source)",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Connect to Scalattice and accept inference jobs
    Connect {
        #[arg(long, env = "SCALATTICE_AGENT_TOKEN")]
        token: Option<String>,
        #[arg(long, env = "SCALATTICE_AGENT_WS")]
        ws: Option<String>,
        #[arg(long, env = "SCALATTICE_AGENT_REGION", default_value = "auto")]
        region: Option<String>,
        #[arg(long, env = "SCALATTICE_AGENT_MODELS")]
        models: Option<String>,
        /// Echo user messages back (for network testing without loaded weights)
        #[arg(long, env = "SCALATTICE_AGENT_DEMO")]
        demo: bool,
    },
    /// Show local GPU detection and configuration hints
    Status {
        #[arg(long, env = "SCALATTICE_AGENT_TOKEN")]
        token: Option<String>,
        #[arg(long, env = "SCALATTICE_AGENT_WS")]
        ws: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Connect {
            token,
            ws,
            region,
            models,
            demo,
        } => {
            let config = config::AgentConfig::from_env_and_cli(token, ws, region, models, demo)?;
            agent::run_agent(config).await?;
        }
        Commands::Status { token, ws } => {
            let specs = specs::detect_machine_specs();
            println!("scalattice-agent {}", env!("CARGO_PKG_VERSION"));
            println!("{}", specs::status_line(&specs));
            let token_set = token
                .or_else(|| std::env::var("SCALATTICE_AGENT_TOKEN").ok())
                .filter(|t| !t.is_empty())
                .is_some();
            println!(
                "token: {}",
                if token_set { "set" } else { "missing (create on Scalattice Cloud → Providers)" }
            );
            let ws_url = ws
                .or_else(|| std::env::var("SCALATTICE_AGENT_WS").ok())
                .unwrap_or_else(|| "wss://api.scalattice.cloud/v1/operators/agent/ws".to_string());
            println!("ws: {ws_url}");
            println!("ready to connect · run: scalattice-agent connect");
        }
    }

    Ok(())
}
