use crate::config::AgentConfig;
use crate::paths::{
    agent_binary_name, agent_env_path, agent_state_path, config_dir, install_dir, lib_dir,
    models_cache_dir, remove_path_quiet, settings_path,
};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;

#[cfg(unix)]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use linux as platform;
#[cfg(windows)]
use windows as platform;

pub enum BackgroundStatus {
    Running,
    Stopped,
    NotInstalled,
}

pub struct UninstallOptions {
    pub yes: bool,
    pub purge_models: bool,
}

pub fn background_status() -> BackgroundStatus {
    platform::background_status()
}

pub fn start_background_from_config(config: &AgentConfig) -> Result<()> {
    platform::start_background_from_config(config)
}

/// Start the background agent when a token is saved but the foreground worker is not running.
pub fn ensure_background_running_if_configured() -> Result<()> {
    if !background_service_available() {
        return Ok(());
    }
    if crate::config::read_saved_agent_token().is_none() {
        return Ok(());
    }
    if service_active() {
        return Ok(());
    }
    match background_status() {
        BackgroundStatus::Running => Ok(()),
        BackgroundStatus::Stopped | BackgroundStatus::NotInstalled => {
            let config = crate::config::AgentConfig::from_env_and_cli(None)?;
            start_background_from_config(&config)
        }
    }
}

#[allow(dead_code)]
pub fn restart_background_from_config(config: &AgentConfig) -> Result<()> {
    platform::restart_background_from_config(config)
}

/// Persist the token and restart background + tray (Windows relaunches the whole app).
#[allow(dead_code)]
pub fn restart_after_token_change(config: &AgentConfig) -> Result<()> {
    platform::restart_after_token_change(config)
}

/// Restart background (+ tray on Windows) from the saved token after an update.
pub fn restart_runtime_from_saved_token() -> Result<()> {
    #[cfg(windows)]
    {
        return platform::restart_runtime_from_saved_token();
    }
    #[cfg(target_os = "linux")]
    {
        return platform::restart_background_after_update();
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        bail!("restart is not supported on this platform")
    }
}

/// Persist the provider token and ensure the background agent is registered + running.
///
/// Always goes through the full start path (not a "already running" short-circuit) so
/// Windows Startup shortcuts are created even when the installer already launched a
/// foreground worker. Skipping that left the tray stuck on "Agent: not set up yet".
pub fn save_agent_token(config: &AgentConfig) -> Result<()> {
    persist_agent_token(&config.token)?;
    if !background_service_available() {
        return Ok(());
    }
    start_background_from_config(config)
}

#[cfg(target_os = "linux")]
pub fn stop_background_for_update() -> Result<()> {
    platform::stop_background_for_update()
}

#[cfg(target_os = "linux")]
pub fn restart_background_after_update() -> Result<()> {
    platform::restart_background_after_update()
}

#[cfg(windows)]
pub fn in_tray_process() -> bool {
    platform::in_tray_process()
}

pub fn invoked_by_systemd() -> bool {
    platform::invoked_by_systemd()
}

pub fn invoked_by_background_service() -> bool {
    platform::invoked_by_background_service()
}

pub fn background_service_available() -> bool {
    platform::background_service_available()
}

pub fn service_active() -> bool {
    platform::service_active()
}

pub fn follow_service_logs(verbose: bool) -> Result<()> {
    platform::follow_service_logs(verbose)
}

#[cfg(windows)]
pub fn autostart_method_line() -> Option<String> {
    platform::autostart_method_line()
}

pub fn persist_agent_token(token: &str) -> Result<bool> {
    let env_file = agent_env_path()?;
    fs::create_dir_all(env_file.parent().context("agent env parent")?)?;

    let mut lines: Vec<String> = if env_file.is_file() {
        fs::read_to_string(&env_file)?
            .lines()
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };

    let key = "SCALATTICE_AGENT_TOKEN";
    let assignment = format!("{key}={token}");
    let mut changed = false;
    let mut found = false;

    for line in &mut lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let assignment_line = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        if let Some((name, _)) = assignment_line.split_once('=') {
            if name.trim() == key {
                found = true;
                if assignment_line != assignment {
                    *line = assignment.clone();
                    changed = true;
                }
            }
        }
    }

    if !found {
        if !lines.is_empty() && !lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
            lines.push(String::new());
        }
        lines.push(assignment);
        changed = true;
    }

    if changed {
        let body = format!("{}\n", lines.join("\n"));
        write_file_replace(&env_file, body.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&env_file, fs::Permissions::from_mode(0o600));
        }
        platform::sync_background_env()?;
    }

    Ok(changed)
}

/// Write via temp + rename, retrying Windows sharing violations (os error 32) when
/// tray/background/AV briefly lock the same config file.
fn write_file_replace(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("config file parent")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("scalattice"),
        std::process::id()
    ));
    let mut last_err = None;
    for attempt in 0..8 {
        match fs::write(&tmp, bytes).and_then(|_| fs::rename(&tmp, path)) {
            Ok(()) => return Ok(()),
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                let retry = err.kind() == std::io::ErrorKind::PermissionDenied
                    || err.raw_os_error() == Some(32) // ERROR_SHARING_VIOLATION
                    || err.raw_os_error() == Some(5); // ERROR_ACCESS_DENIED
                if !retry || attempt == 7 {
                    return Err(err).with_context(|| format!("write {}", path.display()));
                }
                last_err = Some(err);
                std::thread::sleep(std::time::Duration::from_millis(40 * (attempt as u64 + 1)));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("write_file_replace failed")))
        .with_context(|| format!("write {}", path.display()))
}

pub fn uninstall_agent(opts: &UninstallOptions) -> Result<()> {
    let install = install_dir()?;
    let lib = lib_dir()?;
    let config = config_dir()?;
    let models = models_cache_dir();
    let cache_root = models
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| models.clone());

    let mut targets: Vec<PathBuf> = vec![
        install.join(agent_binary_name()),
        lib.clone(),
        agent_env_path()?,
        agent_state_path()?,
        settings_path()?,
    ];

    #[cfg(unix)]
    {
        targets.push(config.join("agent.systemd.env"));
        targets.push(platform::systemd_unit_path()?);
    }

    #[cfg(windows)]
    {
        targets.push(platform::background_runner_path()?);
        let install = install_dir()?;
        targets.push(install.join("scalattice-run.cmd"));
        targets.push(install.join("launch-tray.vbs"));
        targets.push(install.join("launch-tray-interactive.vbs"));
        targets.push(install.join("launch-background.vbs"));
        targets.push(install.join("open-tray-debug.cmd"));
        targets.push(install.join("tray.pid"));
        targets.push(install.join("background.pid"));
        // Logs live under %LOCALAPPDATA%\Scalattice\logs
        if let Ok(log) = crate::paths::agent_log_path() {
            if let Some(logs_dir) = log.parent() {
                targets.push(logs_dir.to_path_buf());
            }
        }
    }

    if opts.purge_models {
        targets.push(models.clone());
        targets.push(cache_root.clone());
    }

    if !opts.yes {
        println!("This will remove Scalattice agent from this machine:");
        if background_service_available() {
            println!("  - stop and disable background agent service");
        }
        for path in &targets {
            println!("  - {}", path.display());
        }
        if !opts.purge_models {
            println!(
                "  (model weights in {} are kept. Add --purge to delete them)",
                models.display()
            );
        }
        bail!("Re-run with --yes to confirm: scalattice-agent uninstall --yes");
    }

    // Always clear autostart (Startup folder + scheduled tasks) and stop processes,
    // even when nothing looks "installed" — leftovers cause reboot Script Host errors.
    let _ = platform::remove_background_service();

    #[cfg(target_os = "linux")]
    {
        let _ = crate::update::sync_auto_update(false);
    }

    for path in &targets {
        remove_path_quiet(path);
    }

    // Wipe remaining config tree (token, state, settings).
    remove_path_quiet(&config);

    #[cfg(windows)]
    {
        // Best-effort wipe of the whole per-user Scalattice appdata tree (bin/lib/logs).
        // The running uninstall binary may remain until Inno deletes {app}.
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let root = PathBuf::from(local).join("Scalattice");
            // Prefer deleting lib/logs first; leave bin for the uninstaller process.
            remove_path_quiet(&root.join("lib"));
            remove_path_quiet(&root.join("logs"));
            if root.is_dir() {
                // Remove any other leftovers under Scalattice except the live bin.
                if let Ok(entries) = fs::read_dir(&root) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.file_name().and_then(|n| n.to_str()) == Some("bin") {
                            continue;
                        }
                        remove_path_quiet(&path);
                    }
                }
            }
        }
    }

    if opts.purge_models {
        remove_path_quiet(&cache_root);
    }

    println!("Scalattice agent uninstalled.");
    if !opts.purge_models && models.is_dir() {
        println!(
            "Model weights kept at {} (re-run with --purge to delete)",
            models.display()
        );
    }
    Ok(())
}
