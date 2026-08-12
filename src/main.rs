#![cfg_attr(windows, windows_subsystem = "windows")]

mod agent;
mod cloud_log;
mod compute_pool;
mod config;
mod hypervisor;
mod inference;
mod llm;
mod logging;
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

#[derive(Parser)]
#[command(
    name = "scalattice-agent",
    about = "Scalattice GPU agent",
    version
)]
struct Cli {
    /// Emit full llama.cpp / GGML detail (default is provider-friendly Simplified logs)
    #[arg(long, global = true)]
    verbose: bool,
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
    /// Internal: tell Scalattice Cloud this machine is uninstalling (best-effort)
    #[command(hide = true)]
    NotifyUninstall,
    /// Windows only: run the notification-area control panel
    #[cfg(windows)]
    Tray {
        /// Start even if another tray instance appears stuck (kills stale tray PID file)
        #[arg(long, hide = true)]
        force: bool,
        /// Show the control panel window (default: tray icon only)
        #[arg(long)]
        open: bool,
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
    /// Restart the background agent (and Windows tray) using the saved token
    Restart,
    /// Internal/elevated: register ONSTART SYSTEM task so the agent runs before sign-in
    #[cfg(windows)]
    #[command(hide = true)]
    InstallBootStart,
    /// Internal: run a per-slot inference worker (spawned by the hypervisor)
    Worker {
        /// JSON WorkerBootConfig (or set SCALATTICE_WORKER_CONFIG)
        #[arg(long)]
        config: Option<String>,
    },
}

fn main() -> Result<()> {
    #[cfg(windows)]
    paths::init_windows_native_search_path();
    init_crypto()?;

    let cli = Cli::parse();
    let verbose = logging::verbose_requested(cli.verbose);

    #[cfg(windows)]
    prepare_windows_process(&cli)?;

    if let Some(Commands::Worker { config }) = &cli.command {
        logging::init_worker_logging(verbose);
        let config = config
            .clone()
            .or_else(|| std::env::var("SCALATTICE_WORKER_CONFIG").ok())
            .ok_or_else(|| {
                anyhow::anyhow!("worker requires --config or SCALATTICE_WORKER_CONFIG")
            })?;
        return hypervisor::run_worker(&config);
    }

    logging::init_logging(verbose);

    #[cfg(windows)]
    if should_run_tray_ui(&cli) {
        let (force, open) = match &cli.command {
            Some(Commands::Tray { force, open }) => (*force, *open),
            _ => (false, false),
        };
        return tray::open_panel(force, open);
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_async(cli, verbose))
}

/// Windows release builds use the WINDOWS subsystem (no console). Attach to a
/// parent console for interactive CLI only - never AllocConsole (that flashes a
/// visible window and can loop with installers / autostart).
#[cfg(windows)]
fn prepare_windows_process(cli: &Cli) -> Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Console::{AttachConsole, FreeConsole, ATTACH_PARENT_PROCESS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let background = matches!(cli.command, Some(Commands::Foreground { .. }))
        && service::invoked_by_background_service();
    let tray = should_run_tray_ui(cli);

    if background {
        // Single-instance: Startup folder + scheduled task can both fire on logon.
        let name: Vec<u16> = "Global\\ScalatticeAgentBackground\0".encode_utf16().collect();
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
            if handle.is_null() {
                anyhow::bail!("failed to create background instance mutex");
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                std::process::exit(0);
            }
            // Keep mutex for process lifetime.
            std::mem::forget(handle);
        }
        write_background_pid();
        unsafe {
            FreeConsole();
        }
        return Ok(());
    }

    if tray {
        unsafe {
            FreeConsole();
        }
        return Ok(());
    }

    // Interactive CLI only: attach to the caller's console. Do not allocate a new one.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
    Ok(())
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

async fn run_async(cli: Cli, verbose: bool) -> Result<()> {
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
            run_foreground(token, verbose).await?;
        }
        Some(Commands::Status) => {
            print_status()?;
            maybe_start_background_from_saved_token()?;
        }
        Some(Commands::SetToken { token }) => {
            let config = config::AgentConfig::from_env_and_cli(Some(token))?;
            // Always use the full save path so Windows Startup registration is not
            // skipped when a background worker is already running.
            let result = service::save_agent_token(&config);
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
        Some(Commands::NotifyUninstall) => {
            service::notify_server_uninstall("uninstall");
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
        Some(Commands::Restart) => {
            service::restart_runtime_from_saved_token()?;
            println!("Scalattice Agent restarted.");
        }
        #[cfg(windows)]
        Some(Commands::InstallBootStart) => {
            service::install_boot_start_elevated()?;
        }
        Some(Commands::Worker { .. }) => unreachable!("worker handled in main"),
    }

    Ok(())
}

#[cfg(windows)]
fn spawn_tray_hidden() -> Result<()> {
    use std::os::windows::process::CommandExt;
    let bin = crate::paths::resolve_agent_binary()?;
    std::process::Command::new(&bin)
        .arg("tray")
        .env("SCALATTICE_TRAY_HIDDEN", "1")
        .env("SCALATTICE_TRAY", "1")
        .creation_flags(0x0800_0000)
        .spawn()
        .context("failed to launch tray")?;
    Ok(())
}

#[cfg(windows)]
fn write_background_pid() {
    if let Ok(dir) = crate::paths::install_dir() {
        let path = dir.join("background.pid");
        let _ = std::fs::write(&path, format!("{}", std::process::id()));
    }
}

#[cfg(windows)]
fn clear_background_pid() {
    if let Ok(dir) = crate::paths::install_dir() {
        let path = dir.join("background.pid");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if raw.trim() == std::process::id().to_string() {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

async fn run_foreground(token: Option<String>, verbose: bool) -> Result<()> {
    if service::invoked_by_systemd() || service::invoked_by_background_service() {
        let _ = update::maybe_sync_auto_update_timer();
        let token = token
            .filter(|t| !t.trim().is_empty())
            .or_else(config::read_saved_agent_token);
        let config = config::AgentConfig::from_env_and_cli(token)?;
        let result = agent::run_agent(config).await;
        #[cfg(windows)]
        clear_background_pid();
        return result;
    }

    if service::background_service_available() {
        if !service::service_active() {
            maybe_start_background_from_saved_token()?;
        }
        if service::service_active() {
            if verbose {
                println!("following background agent · verbose (Ctrl+C to stop watching only)");
            } else {
                println!("following background agent · simplified (Ctrl+C to stop watching only; use --verbose for full detail)");
            }
            return service::follow_service_logs(verbose);
        }
        anyhow::bail!("agent not running. Run: scalattice-agent set-token --token slt_provider_...");
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
        println!("         scalattice-agent set-token --token slt_provider_...");
    }

    if service::background_service_available() {
        #[cfg(windows)]
        {
            let service_line = match service::background_status() {
                service::BackgroundStatus::Running => "running",
                service::BackgroundStatus::Stopped => {
                    if cfg!(windows) {
                        "stopped (starts at boot / when you sign in)"
                    } else {
                        "stopped"
                    }
                }
                service::BackgroundStatus::NotInstalled => "not set up",
            };
            println!("Agent    {service_line}");
            if let Some(line) = service::autostart_method_line() {
                println!("Autostart {line}");
            }
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
    #[cfg(not(windows))]
    {
        if let Ok(bin) = crate::paths::install_dir() {
            println!("Bin      {}", bin.display());
        }
        if let Ok(lib) = crate::paths::lib_dir() {
            println!("Lib      {}", lib.display());
        }
    }
    if let Ok(log) = crate::paths::agent_log_path() {
        println!("Log file {}", log.display());
        #[cfg(not(windows))]
        if let Some(parent) = log.parent() {
            let tray_log = parent.join("tray.log");
            if tray_log.is_file() {
                println!("Tray log {}", tray_log.display());
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
