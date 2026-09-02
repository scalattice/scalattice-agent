use crate::config::AgentConfig;
use crate::paths::{
    agent_binary_name, agent_env_path, agent_state_path, config_dir, install_dir, lib_dir,
    models_cache_dir, remove_path_quiet, settings_path,
};
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod uninstall_notify;
#[cfg(windows)]
mod windows;

/// Best-effort cloud notify used by the Inno uninstaller before process kill.
pub fn notify_server_uninstall(reason: &str) {
    uninstall_notify::notify_server_uninstall(reason);
}

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
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

#[cfg(any(windows, target_os = "macos"))]
pub fn restart_background_from_config(config: &AgentConfig) -> Result<()> {
    platform::restart_background_from_config(config)
}

/// Restart background (+ tray on Windows) from the saved token after an update.
pub fn restart_runtime_from_saved_token() -> Result<()> {
    #[cfg(windows)]
    {
        return platform::restart_runtime_from_saved_token();
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        return platform::restart_background_after_update();
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
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
    start_background_from_config(config)?;
    #[cfg(windows)]
    {
        // Interactive set-token / tray Save only: may show UAC once if boot task missing.
        let _ = platform::ensure_boot_start_interactive();
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn stop_background_for_update() -> Result<()> {
    platform::stop_background_for_update()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn restart_background_after_update() -> Result<()> {
    platform::restart_background_after_update()
}

#[cfg(target_os = "macos")]
pub fn sync_macos_auto_update(enable: bool) -> Result<()> {
    platform::sync_update_launch_agent(enable)
}

#[cfg(target_os = "macos")]
pub fn ensure_tray_login_item() -> Result<()> {
    platform::ensure_tray_login_item()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn in_tray_process() -> bool {
    platform::in_tray_process()
}

#[cfg(windows)]
pub fn install_boot_start_elevated() -> Result<()> {
    platform::install_boot_start_elevated()
}

#[cfg(windows)]
pub(crate) fn prevent_stdio_handle_inheritance() {
    platform::prevent_stdio_handle_inheritance();
}

/// Background single-instance lock. `Ok(false)` = another instance already owns it.
#[cfg(windows)]
pub fn try_acquire_background_instance_mutex() -> Result<bool> {
    platform::try_acquire_background_instance_mutex()
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

    // Windows Notepad/PowerShell often save this file as UTF-16. `read_to_string`
    // then fails with "stream did not contain valid UTF-8" and the tray cannot save.
    let (mut lines, mut changed) = if env_file.is_file() {
        let bytes = fs::read(&env_file).with_context(|| format!("read {}", env_file.display()))?;
        let raw = crate::config::decode_text_bytes(&bytes);
        let rewrite = crate::config::text_file_needs_utf8_rewrite(&bytes, &raw);
        (raw.lines().map(str::to_string).collect::<Vec<_>>(), rewrite)
    } else {
        (Vec::new(), false)
    };

    let key = "SCALATTICE_AGENT_TOKEN";
    let assignment = format!("{key}={token}");
    let mut found = false;

    for line in &mut lines {
        let trimmed = line.trim().trim_start_matches('\u{feff}').trim();
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

pub fn uninstall_agent(mut opts: UninstallOptions) -> Result<()> {
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

    #[cfg(target_os = "linux")]
    {
        targets.push(config.join("agent.systemd.env"));
        targets.push(platform::systemd_unit_path()?);
        if let Ok(log) = crate::paths::agent_log_path() {
            if let Some(logs_dir) = log.parent() {
                targets.push(logs_dir.to_path_buf());
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        targets.push(platform::systemd_unit_path()?);
        if let Ok(home) = crate::paths::home_dir() {
            targets.push(home.join("Library/LaunchAgents/com.scalattice.agent.update.plist"));
            targets.push(home.join("Library/LaunchAgents/com.scalattice.agent.tray.plist"));
        }
        if let Ok(log) = crate::paths::agent_log_path() {
            if let Some(logs_dir) = log.parent() {
                targets.push(logs_dir.to_path_buf());
            }
        }
    }

    #[cfg(windows)]
    {
        targets.push(platform::background_runner_path()?);
        let install = install_dir()?;
        targets.push(install.join("scalattice-run.cmd"));
        targets.push(install.join("launch-tray.vbs"));
        targets.push(install.join("launch-tray-interactive.vbs"));
        targets.push(install.join("launch-background.vbs"));
        targets.push(install.join("launch-background-delayed.vbs"));
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

    if !opts.yes {
        println!("This will remove Scalattice agent from this machine:");
        if background_service_available() {
            println!("  - stop and disable background agent service");
        }
        for path in &targets {
            println!("  - {}", path.display());
        }
        if std::io::stdin().is_terminal() {
            if models.is_dir() && !opts.purge_models {
                print!(
                    "Also delete downloaded models in {}? [y/N] ",
                    models.display()
                );
                let _ = std::io::stdout().flush();
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                let answer = line.trim().to_ascii_lowercase();
                opts.purge_models = answer == "y" || answer == "yes";
            }
            print!("Type yes to uninstall: ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !line.trim().eq_ignore_ascii_case("yes") {
                bail!("Uninstall cancelled.");
            }
        } else {
            if !opts.purge_models {
                println!(
                    "  (model weights in {} are kept. Add --purge to delete them)",
                    models.display()
                );
            }
            bail!("Re-run with --yes to confirm: scalattice-agent uninstall --yes");
        }
    }

    if opts.purge_models {
        targets.push(models.clone());
        targets.push(cache_root.clone());
    }

    // Tell Scalattice Cloud this machine is going away (best-effort, before wipe).
    uninstall_notify::notify_server_uninstall("uninstall");

    // Always clear autostart (Startup folder + scheduled tasks) and stop processes,
    // even when nothing looks "installed": leftovers cause reboot Script Host errors.
    let _ = platform::remove_background_service();

    #[cfg(any(target_os = "linux", target_os = "macos"))]
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
