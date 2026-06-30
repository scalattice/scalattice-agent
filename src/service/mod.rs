use crate::config::AgentConfig;
use crate::paths::{
    agent_binary_name, agent_env_path, agent_state_path, config_dir, install_dir, is_dir_empty,
    lib_dir, models_cache_dir, remove_path_quiet, settings_path,
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

pub fn restart_background_from_config(config: &AgentConfig) -> Result<()> {
    platform::restart_background_from_config(config)
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

pub fn follow_service_logs() -> Result<()> {
    platform::follow_service_logs()
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
        fs::write(&env_file, format!("{}\n", lines.join("\n")))?;
        platform::sync_background_env()?;
    }

    Ok(changed)
}

pub fn uninstall_agent(opts: &UninstallOptions) -> Result<()> {
    let install = install_dir()?;
    let lib = lib_dir()?;
    let config = config_dir()?;
    let models = models_cache_dir();

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
        targets.push(install.join("launch-background.vbs"));
    }

    if opts.purge_models {
        targets.push(models.clone());
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

    if background_service_available() {
        platform::remove_background_service()?;
    }

    #[cfg(target_os = "linux")]
    {
        let _ = crate::update::sync_auto_update(false);
    }

    for path in &targets {
        remove_path_quiet(path);
    }

    if config.is_dir() && is_dir_empty(&config) {
        let _ = fs::remove_dir(&config);
    }

    let cache_dir = models.parent().map(|p| p.to_path_buf());
    if opts.purge_models {
        if let Some(cache_dir) = cache_dir {
            if cache_dir.is_dir() && is_dir_empty(&cache_dir) {
                let _ = fs::remove_dir(&cache_dir);
            }
        }
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
