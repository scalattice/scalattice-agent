mod agent;
mod compute_pool;
mod config;
mod inference;
mod llm;
mod models;
mod paths;
mod protocol;
mod runtime;
mod service;
mod specs;
mod state;
#[cfg(windows)]
mod tray;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "scalattice-agent",
    about = "Scalattice GPU operator agent",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
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
    /// Windows only: run the notification-area control panel
    #[cfg(windows)]
    Tray {
        /// Start even if another tray instance appears stuck (kills stale tray PID file)
        #[arg(long, hide = true)]
        force: bool,
    },
}

fn main() -> Result<()> {
    init_crypto()?;
    init_logging();

    let cli = Cli::parse();

    #[cfg(windows)]
    if should_run_tray_ui(&cli) {
        let force = matches!(&cli.command, Some(Commands::Tray { force: true }));
        return tray::open_panel(force);
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_async(cli))
}

#[cfg(windows)]
fn should_run_tray_ui(cli: &Cli) -> bool {
    matches!(cli.command, None | Some(Commands::Tray { .. }))
}

fn init_crypto() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install rustls crypto provider"))
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .init();
}

async fn run_async(cli: Cli) -> Result<()> {
    match cli.command {
        None => {
            #[cfg(not(windows))]
            anyhow::bail!(
                "no command specified; try: scalattice-agent status | foreground | set-token"
            );
            #[cfg(windows)]
            unreachable!("tray handled in main")
        }
        Some(Commands::Foreground { token }) => {
            run_foreground(token).await?;
        }
        Some(Commands::Status) => {
            print_status()?;
            maybe_start_background_from_saved_token()?;
        }
        Some(Commands::SetToken { token }) => {
            let config = config::AgentConfig::from_env_and_cli(Some(token))?;
            service::persist_agent_token(&config.token)?;
            match service::start_background_from_config(&config) {
                Ok(()) => {
                    if service::service_active() {
                        println!("Token saved. Background agent running.");
                    } else {
                        println!("Token saved. Agent will start at next logon (Startup folder).");
                    }
                }
                Err(err) => {
                    println!("Token saved.");
                    eprintln!("Note: {err}");
                }
            }
            #[cfg(windows)]
            spawn_tray_hidden()?;
        }
        Some(Commands::Uninstall { yes, purge }) => {
            service::uninstall_agent(&service::UninstallOptions {
                yes,
                purge_models: purge,
            })?;
        }
        #[cfg(windows)]
        Some(Commands::Tray { .. }) => unreachable!("tray handled in main"),
    }

    Ok(())
}

#[cfg(windows)]
fn spawn_tray_hidden() -> Result<()> {
    use std::os::windows::process::CommandExt;
    let vbs = crate::paths::install_dir()?.join("launch-tray.vbs");
    std::process::Command::new("wscript.exe")
        .args(["//nologo", &vbs.display().to_string()])
        .creation_flags(0x0800_0000)
        .spawn()
        .context("failed to launch tray")?;
    Ok(())
}

async fn run_foreground(token: Option<String>) -> Result<()> {
    if service::invoked_by_systemd() || service::invoked_by_background_service() {
        let config = config::AgentConfig::from_env_and_cli(token)?;
        return agent::run_agent(config).await;
    }

    if service::background_service_available() {
        if !service::service_active() {
            maybe_start_background_from_saved_token()?;
        }
        if service::service_active() {
            println!("following background agent (Ctrl+C to stop watching only)");
            return service::follow_service_logs();
        }
        anyhow::bail!("agent not running. Run: scalattice-agent set-token --token slt_provider_…");
    }

    let config = config::AgentConfig::from_env_and_cli(token)?;
    agent::run_agent(config).await
}

fn maybe_start_background_from_saved_token() -> Result<()> {
    if !service::background_service_available() {
        return Ok(());
    }
    if !agent_token_configured() {
        return Ok(());
    }
    if service::service_active() {
        return Ok(());
    }
    match service::background_status() {
        service::BackgroundStatus::Running => Ok(()),
        service::BackgroundStatus::Stopped | service::BackgroundStatus::NotInstalled => {
            let config = config::AgentConfig::from_env_and_cli(None)?;
            if let Err(err) = service::start_background_from_config(&config) {
                if !service::service_active() {
                    eprintln!("Note: could not auto-start background agent: {err}");
                }
            } else if service::service_active() {
                println!("Background agent started.");
            }
            Ok(())
        }
    }
}

fn print_status() -> Result<()> {
    println!("scalattice-agent {}", env!("CARGO_PKG_VERSION"));
    println!();

    let cloud_line = state::cloud_connection_line();
    let cloud = cloud_line
        .strip_prefix("Scalattice Cloud: ")
        .unwrap_or(cloud_line.as_str());
    println!("Cloud    {cloud}");

    if agent_token_configured() {
        println!("Token    set");
    } else {
        println!("Token    not set");
        println!("         Create one at https://scalattice.cloud/providers");
        println!("         scalattice-agent set-token --token slt_provider_…");
    }

    if service::background_service_available() {
        let service_line = match service::background_status() {
            service::BackgroundStatus::Running => "running",
            service::BackgroundStatus::Stopped => "stopped",
            service::BackgroundStatus::NotInstalled => "not configured",
        };
        println!("Service  {service_line}");
        #[cfg(windows)]
        if let Some(method) = service::autostart_method_line() {
            println!("Autostart {method}");
        }
    } else {
        #[cfg(unix)]
        println!("Service  systemd unavailable (use: scalattice-agent foreground)");
        #[cfg(windows)]
        println!("Service  task scheduler unavailable (use: scalattice-agent foreground)");
        #[cfg(not(any(unix, windows)))]
        println!("Service  background mode unavailable (use: scalattice-agent foreground)");
    }

    if agent_token_configured() {
        if let Some(summary) = state::agent_activity_summary() {
            println!("Status   {}", summary.status);
            if let Some(node) = summary.node_id {
                println!("Node     {node}");
            }
        }
    }

    println!();
    if let Ok(bin) = crate::paths::install_dir() {
        println!("Bin      {}", bin.display());
    }
    if let Ok(lib) = crate::paths::lib_dir() {
        println!("Lib      {}", lib.display());
    }
    if let Ok(log) = crate::paths::agent_log_path() {
        println!("Log      {}", log.display());
        if let Some(parent) = log.parent() {
            let tray_log = parent.join("tray.log");
            if tray_log.is_file() {
                println!("Tray log {}", tray_log.display());
            }
        }
    }
    println!();
    println!("Dashboard https://scalattice.cloud/providers");
    #[cfg(windows)]
    println!("Control panel: scalattice-agent tray  (or click the notification-area icon)");
    Ok(())
}

fn agent_token_configured() -> bool {
    config::read_saved_agent_token().is_some()
}
