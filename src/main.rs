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
mod settings;
mod specs;
mod state;
mod update;
mod vram_lifecycle;
#[cfg(windows)]
mod tray;

use anyhow::Result;
#[cfg(windows)]
use anyhow::Context;
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
        /// Provider token (background launcher passes this; do not rely on SCALATTICE_AGENT_TOKEN env)
        #[arg(long, hide = true)]
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
    /// Check for and install the latest release
    Update {
        /// Only check whether a newer release exists
        #[arg(long)]
        check: bool,
        /// Enable automatic daily update checks (Linux: systemd user timer)
        #[arg(long)]
        enable_auto: bool,
        /// Disable automatic daily update checks
        #[arg(long)]
        disable_auto: bool,
    },
}

fn main() -> Result<()> {
    #[cfg(windows)]
    paths::init_windows_native_search_path();
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
            let result = if service::service_active() {
                service::save_agent_token(&config)
            } else {
                service::restart_after_token_change(&config)
            };
            match result {
                Ok(()) => {
                    if service::service_active() {
                        println!("Token saved. Agent is reconnecting.");
                    } else {
                        println!("Token saved. Scalattice Agent is starting.");
                    }
                }
                Err(err) => {
                    println!("Token saved.");
                    eprintln!("Note: {err}");
                    #[cfg(windows)]
                    spawn_tray_hidden()?;
                }
            }
            let _ = update::maybe_sync_auto_update_timer();
        }
        Some(Commands::Uninstall { yes, purge }) => {
            service::uninstall_agent(&service::UninstallOptions {
                yes,
                purge_models: purge,
            })?;
        }
        #[cfg(windows)]
        Some(Commands::Tray { .. }) => unreachable!("tray handled in main"),
        Some(Commands::Update {
            check,
            enable_auto,
            disable_auto,
        }) => {
            run_update(check, enable_auto, disable_auto).await?;
        }
    }

    Ok(())
}

#[cfg(windows)]
fn spawn_tray_hidden() -> Result<()> {
    use std::os::windows::process::CommandExt;
    let bin = crate::paths::resolve_agent_binary()?;
    std::process::Command::new(&bin)
        .arg("tray")
        .creation_flags(0x0800_0000)
        .spawn()
        .context("failed to launch tray")?;
    Ok(())
}

async fn run_foreground(token: Option<String>) -> Result<()> {
    if service::invoked_by_systemd() || service::invoked_by_background_service() {
        let _ = update::maybe_sync_auto_update_timer();
        let token = token
            .filter(|t| !t.trim().is_empty())
            .or_else(config::read_saved_agent_token);
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
    match service::ensure_background_running_if_configured() {
        Ok(()) => {
            if service::service_active() {
                println!("Background agent started.");
            }
            Ok(())
        }
        Err(err) => {
            if !service::service_active() {
                eprintln!("Note: could not auto-start background agent: {err}");
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
        #[cfg(windows)]
        {
            let service_line = match service::background_status() {
                service::BackgroundStatus::Running => "running",
                service::BackgroundStatus::Stopped => "stopped (starts when you sign in)",
                service::BackgroundStatus::NotInstalled => "not set up",
            };
            println!("Agent    {service_line}");
        }
        #[cfg(not(windows))]
        {
            let service_line = match service::background_status() {
                service::BackgroundStatus::Running => "running",
                service::BackgroundStatus::Stopped => "stopped",
                service::BackgroundStatus::NotInstalled => "not configured",
            };
            println!("Service  {service_line}");
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
    #[cfg(windows)]
    {
        if let Ok(log) = crate::paths::agent_log_path() {
            println!("Log file {}", log.display());
        }
    }
    #[cfg(not(windows))]
    {
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
    }
    println!();
    println!("Dashboard https://scalattice.cloud/providers");
    let user_settings = settings::UserSettings::load();
    #[cfg(windows)]
    {
        if user_settings.auto_update {
            println!("Update   automatic");
        } else {
            println!("Update   scalattice-agent update  (or use the panel Updates section)");
        }
        println!("Control panel: scalattice-agent tray  (or click the notification-area icon)");
    }
    #[cfg(target_os = "linux")]
    {
        if user_settings.auto_update {
            println!("Update   automatic");
        } else {
            println!("Update   scalattice-agent update");
            println!("         scalattice-agent update --enable-auto");
        }
    }
    Ok(())
}

fn agent_token_configured() -> bool {
    config::read_saved_agent_token().is_some()
}

async fn run_update(check_only: bool, enable_auto: bool, disable_auto: bool) -> Result<()> {
    if enable_auto && disable_auto {
        anyhow::bail!("cannot use --enable-auto and --disable-auto together");
    }

    let mut user_settings = settings::UserSettings::load();

    if enable_auto {
        user_settings.auto_update = true;
        user_settings.save()?;
        update::sync_auto_update(true)?;
        return Ok(());
    }
    if disable_auto {
        user_settings.auto_update = false;
        user_settings.save()?;
        update::sync_auto_update(false)?;
        return Ok(());
    }

    let outcome = update::check_for_update().await?;
    println!("{}", update::format_update_status(&outcome));

    user_settings.mark_update_checked();
    let _ = user_settings.save();

    if check_only || !outcome.info().update_available {
        return Ok(());
    }

    update::install_latest_update().await
}
