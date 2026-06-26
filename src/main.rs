mod agent;
mod config;
mod protocol;
mod runtime;
mod service;
mod specs;

use anyhow::Result;
use clap::{Parser, Subcommand};
use runtime::demo_mode_from_env;
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
        /// Echo user messages back (for network testing without loaded weights).
        /// Also enabled when SCALATTICE_AGENT_DEMO=1 (read in config, not via clap env).
        #[arg(long)]
        demo: bool,
    },
    /// Show local GPU detection and configuration hints
    Status {
        #[arg(long, env = "SCALATTICE_AGENT_TOKEN")]
        token: Option<String>,
        #[arg(long, env = "SCALATTICE_AGENT_WS")]
        ws: Option<String>,
    },
    /// Install or manage a background systemd user service
    Service {
        #[command(subcommand)]
        command: ServiceCommands,
    },
}

#[derive(Subcommand)]
enum ServiceCommands {
    /// Write and enable a user systemd unit (auto-restart on disconnect)
    Install,
    /// Disable and remove the user systemd unit
    Uninstall,
    /// Show whether the background service is installed and running
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install rustls crypto provider"))?;

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
            print_status(token, ws)?;
        }
        Commands::Service { command } => match command {
            ServiceCommands::Install => service::install_user_service()?,
            ServiceCommands::Uninstall => service::uninstall_user_service()?,
            ServiceCommands::Status => service::service_status()?,
        },
    }

    Ok(())
}

fn print_status(token: Option<String>, ws: Option<String>) -> Result<()> {
    let specs = specs::detect_machine_specs();
    let demo = demo_mode_from_env();

    println!("scalattice-agent {}", env!("CARGO_PKG_VERSION"));
    println!("{}", specs::status_line(&specs));
    println!(
        "mode: {}",
        if demo {
            "demo (SCALATTICE_AGENT_DEMO=1 — echo only, no real inference)"
        } else {
            "production (model weights must be loaded for real inference)"
        }
    );
    println!(
        "inference ready: {}",
        if demo {
            "yes — demo echo responses"
        } else {
            "no — pull model weights locally, or set SCALATTICE_AGENT_DEMO=1 to test connectivity"
        }
    );

    let token_set = token
        .or_else(|| std::env::var("SCALATTICE_AGENT_TOKEN").ok())
        .filter(|t| !t.is_empty())
        .is_some();
    println!(
        "token: {}",
        if token_set {
            "set"
        } else {
            "missing (create on Scalattice Cloud → Providers)"
        }
    );

    let ws_url = ws
        .or_else(|| std::env::var("SCALATTICE_AGENT_WS").ok())
        .unwrap_or_else(|| "wss://api.scalattice.cloud/v1/operators/agent/ws".to_string());
    println!("ws: {ws_url}");
    println!();
    println!("Foreground:  scalattice-agent connect");
    println!("Background:  scalattice-agent service install");
    Ok(())
}
