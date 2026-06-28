mod agent;
mod compute_pool;
mod config;
mod inference;
mod llm;
mod models;
mod protocol;
mod runtime;
mod service;
mod specs;
mod state;

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
    /// Follow live logs from the background agent (Ctrl+C stops watching only)
    Foreground {
        #[arg(long, env = "SCALATTICE_AGENT_TOKEN", hide = true)]
        token: Option<String>,
    },
    /// Show connection status and whether the background agent is running
    Status,
    /// Save the machine token and start (or restart) the background agent
    SetToken {
        #[arg(long, env = "SCALATTICE_AGENT_TOKEN")]
        token: String,
    },
    /// Remove the agent, background service, config, and bundled libraries
    Uninstall {
        /// Confirm removal (required)
        #[arg(long, short = 'y')]
        yes: bool,
        /// Also delete downloaded model weights (~/.cache/scalattice/models)
        #[arg(long)]
        purge: bool,
    },
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
        Commands::Foreground { token } => {
            run_foreground(token).await?;
        }
        Commands::Status => {
            print_status()?;
            maybe_start_background_from_saved_token()?;
        }
        Commands::SetToken { token } => {
            let config = config::AgentConfig::from_env_and_cli(Some(token))?;
            service::persist_agent_token(&config.token)?;
            if service::systemd_available() {
                service::start_background_from_config(&config)?;
                println!("token saved — agent running in background");
            } else {
                println!("token saved — run: scalattice-agent foreground");
            }
        }
        Commands::Uninstall { yes, purge } => {
            service::uninstall_agent(&service::UninstallOptions {
                yes,
                purge_models: purge,
            })?;
        }
    }

    Ok(())
}

async fn run_foreground(token: Option<String>) -> Result<()> {
    // systemd ExecStart — this process IS the background agent.
    if service::invoked_by_systemd() {
        let config = config::AgentConfig::from_env_and_cli(token)?;
        return agent::run_agent(config).await;
    }

    // User command — watch the running agent without touching it.
    if service::systemd_available() {
        if !service::service_active() {
            maybe_start_background_from_saved_token()?;
        }
        if service::service_active() {
            println!("following background agent (Ctrl+C to stop watching only)");
            return service::follow_service_logs();
        }
        anyhow::bail!("agent not running — run: scalattice-agent set-token --token slt_provider_…");
    }

    // No systemd (dev machines): run the agent in this terminal.
    let config = config::AgentConfig::from_env_and_cli(token)?;
    agent::run_agent(config).await
}

/// If a token is saved but the background unit is missing or stopped, start it.
fn maybe_start_background_from_saved_token() -> Result<()> {
    if !service::systemd_available() {
        return Ok(());
    }
    if !agent_token_configured() {
        return Ok(());
    }
    match service::background_status() {
        service::BackgroundStatus::Running => Ok(()),
        service::BackgroundStatus::Stopped | service::BackgroundStatus::NotInstalled => {
            let config = config::AgentConfig::from_env_and_cli(None)?;
            service::start_background_from_config(&config)?;
            println!("background: started");
            Ok(())
        }
    }
}

fn print_status() -> Result<()> {
    println!("scalattice-agent {}", env!("CARGO_PKG_VERSION"));
    println!("{}", state::cloud_connection_line());

    if !agent_token_configured() {
        println!("token: not set — create one at https://scalattice.cloud/providers");
        println!("set token: scalattice-agent set-token --token slt_provider_…");
    } else {
        println!("token: set");
    }

    if service::systemd_available() {
        match service::background_status() {
            service::BackgroundStatus::Running => println!("background: running"),
            service::BackgroundStatus::Stopped => println!("background: not running"),
            service::BackgroundStatus::NotInstalled => {
                println!("background: not started — run: scalattice-agent set-token --token …");
            }
        }
    } else {
        println!("background: systemd not available (use: scalattice-agent foreground)");
    }

    if agent_token_configured() {
        println!("activity: {}", state::server_status_line());
    }

    println!("dashboard: https://scalattice.cloud/providers");
    println!("manage GPUs, models, and jobs in the provider dashboard");
    Ok(())
}

fn agent_token_configured() -> bool {
    config::read_saved_agent_token().is_some()
}
