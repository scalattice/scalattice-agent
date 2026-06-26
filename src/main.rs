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
        #[arg(long, env = "SCALATTICE_AGENT_WS")]
        ws: Option<String>,
        #[arg(long, env = "SCALATTICE_AGENT_REGION", default_value = "auto")]
        region: Option<String>,
        #[arg(long, env = "SCALATTICE_AGENT_MODELS")]
        models: Option<String>,
        /// Run in the foreground instead of the background systemd service
        #[arg(long)]
        foreground: bool,
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
    /// Update the machine token in agent.env and restart the background service
    SetToken {
        #[arg(long, env = "SCALATTICE_AGENT_TOKEN")]
        token: String,
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
            foreground,
        } => {
            let config = config::AgentConfig::from_env_and_cli(token, ws, region, models)?;
            if foreground {
                agent::run_agent(config).await?;
            } else {
                service::ensure_service_running(&config)?;
            }
        }
        Commands::Status { token, ws } => {
            print_status(token, ws)?;
        }
        Commands::Service { command } => match command {
            ServiceCommands::Install => service::install_user_service()?,
            ServiceCommands::Uninstall => service::uninstall_user_service()?,
            ServiceCommands::Status => service::service_status()?,
        },
        Commands::SetToken { token } => {
            let config = config::AgentConfig::from_env_and_cli(Some(token), None, None, None)?;
            service::persist_agent_token(&config.token)?;
            if service::systemd_available() && service::service_active() {
                service::restart_user_service()?;
            }
            println!("token updated in agent.env");
        },
    }

    Ok(())
}

fn print_status(token: Option<String>, ws: Option<String>) -> Result<()> {
    let specs = specs::detect_machine_specs();

    println!("scalattice-agent {}", env!("CARGO_PKG_VERSION"));
    println!("{}", specs::status_line(&specs));
    println!("server: {}", state::server_status_line());
    println!("demo mode: {}", state::demo_status_line());

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

    let cached = models::list_cached_runtime_models();
    println!("models cache: {}", models::models_dir().display());
    if cached.is_empty() {
        println!("models: none downloaded yet (Scalattice pushes weights on connect)");
    } else {
        println!("models: {}", cached.join(", "));
    }

    if service::systemd_available() {
        match service::service_active() {
            true => println!("service: running"),
            false => println!("service: not running"),
        }
    }

    if specs.compute_devices.len() > 1 {
        println!();
        println!("compute devices:");
        for device in &specs.compute_devices {
            let kind = match device.kind.as_str() {
                "cpu" => "CPU",
                "integrated" => "integrated GPU",
                _ => "GPU",
            };
            let enabled = if device.enabled { "enabled" } else { "disabled (dashboard)" };
            println!("  - {} ({kind}) · {enabled}", device.name);
        }
    }

    println!();
    if service::systemd_available() {
        println!("connect:    scalattice-agent connect              (background service)");
        println!("foreground: scalattice-agent connect --foreground");
    } else {
        println!("connect:    scalattice-agent connect --foreground");
    }
    Ok(())
}
