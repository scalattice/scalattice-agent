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
    /// Connect to Scalattice and accept inference jobs (background service by default)
    Connect {
        #[arg(long, env = "SCALATTICE_AGENT_TOKEN")]
        token: Option<String>,
        /// Follow background service logs (default when the service is already running)
        #[arg(long)]
        foreground: bool,
    },
    /// Show whether this machine is connected to Scalattice Cloud
    Status,
    /// Install or manage a background systemd user service
    Service {
        #[command(subcommand)]
        command: ServiceCommands,
    },
    /// Update the machine token in agent.env and restart the background service
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
        Commands::Connect { token, foreground } => {
            if foreground && service::service_active() {
                println!("scalattice-agent is running in the background");
                service::follow_service_logs()?;
                return Ok(());
            }

            let config = config::AgentConfig::from_env_and_cli(token)?;
            if foreground {
                agent::run_agent(config).await?;
            } else {
                service::ensure_service_running(&config)?;
            }
        }
        Commands::Status => {
            print_status()?;
        }
        Commands::Service { command } => match command {
            ServiceCommands::Install => service::install_user_service()?,
            ServiceCommands::Uninstall => service::uninstall_user_service()?,
            ServiceCommands::Status => service::service_status()?,
        },
        Commands::SetToken { token } => {
            let config = config::AgentConfig::from_env_and_cli(Some(token))?;
            service::persist_agent_token(&config.token)?;
            if service::systemd_available() && service::service_active() {
                service::restart_user_service()?;
            }
            println!("token updated in agent.env");
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

fn print_status() -> Result<()> {
    println!("scalattice-agent {}", env!("CARGO_PKG_VERSION"));
    println!("{}", state::cloud_connection_line());

    if !agent_token_configured() {
        println!("token: not set — create one at https://scalattice.cloud/providers");
        println!("set token: scalattice-agent set-token --token slt_provider_…");
    } else {
        println!("token: set");
    }

    println!("dashboard: https://scalattice.cloud/providers");
    println!("manage GPUs, models, and jobs in the provider dashboard");
    Ok(())
}

fn agent_token_configured() -> bool {
    config::read_saved_agent_token().is_some()
}
