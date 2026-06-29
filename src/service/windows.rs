use crate::config::AgentConfig;
use crate::paths::{agent_binary_name, agent_log_path, install_dir, lib_dir, resolve_agent_binary};
use crate::service::BackgroundStatus;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const TASK_NAME: &str = "ScalatticeAgent";
const TRAY_TASK_NAME: &str = "ScalatticeAgentTray";

pub fn background_status() -> BackgroundStatus {
    if !background_service_available() {
        return BackgroundStatus::NotInstalled;
    }
    if !task_exists() {
        return BackgroundStatus::NotInstalled;
    }
    if service_active() {
        BackgroundStatus::Running
    } else {
        BackgroundStatus::Stopped
    }
}

pub fn start_background_from_config(config: &AgentConfig) -> Result<()> {
    ensure_background_task(config)
}

pub fn invoked_by_systemd() -> bool {
    false
}

pub fn invoked_by_background_service() -> bool {
    std::env::var("SCALATTICE_BACKGROUND")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

pub fn background_service_available() -> bool {
    Command::new("schtasks")
        .arg("/?")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn service_active() -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {}", agent_binary_name()), "/NH"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.contains(agent_binary_name())
        }
        _ => false,
    }
}

pub fn follow_service_logs() -> Result<()> {
    if !service_active() {
        bail!(
            "scalattice-agent is not running; save your token with `scalattice-agent set-token` first"
        );
    }

    let log_path = agent_log_path()?;
    if !log_path.is_file() {
        bail!(
            "log file not found at {} (the agent may still be starting)",
            log_path.display()
        );
    }

    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Get-Content -Path '{}' -Wait -Tail 30",
                log_path.display().to_string().replace('\'', "''")
            ),
        ])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("failed to run powershell log tail")?;

    match status.code() {
        Some(0) | Some(130) => Ok(()),
        Some(code) => bail!("log tail exited with status {code}"),
        None => bail!("log tail terminated by signal"),
    }
}

pub fn sync_background_env() -> Result<()> {
    write_background_runner().map(|_| ())
}

pub fn remove_background_service() -> Result<()> {
    let _ = Command::new("schtasks")
        .args(["/End", "/TN", TASK_NAME])
        .output();
    let _ = Command::new("schtasks")
        .args(["/End", "/TN", TRAY_TASK_NAME])
        .output();
    let _ = Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .output();
    let _ = Command::new("schtasks")
        .args(["/Delete", "/TN", TRAY_TASK_NAME, "/F"])
        .output();
    let _ = Command::new("taskkill")
        .args(["/F", "/IM", agent_binary_name()])
        .output();
    Ok(())
}

pub fn background_runner_path() -> Result<PathBuf> {
    Ok(install_dir()?.join("run-background.cmd"))
}

pub fn systemd_unit_path() -> Result<PathBuf> {
    background_runner_path()
}

fn ensure_background_task(config: &AgentConfig) -> Result<()> {
    let token_changed = crate::service::persist_agent_token(&config.token)?;
    let runner_changed = write_background_runner()?;

    if !task_exists() || token_changed || runner_changed {
        create_or_update_task()?;
    } else if !service_active() {
        run_task_now()?;
    }

    ensure_tray_task()?;
    run_tray_now()?;

    Ok(())
}

fn write_background_runner() -> Result<bool> {
    let install = install_dir()?;
    let lib = lib_dir()?;
    let log = agent_log_path()?;
    let bin = resolve_agent_binary()?;
    let runner = install.join("run-background.cmd");

    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir_all(&install)?;

    let script = format!(
        "@echo off\r\n\
set SCALATTICE_BACKGROUND=1\r\n\
set \"PATH={install};{lib};%PATH%\"\r\n\
cd /d \"{install}\"\r\n\
\"{bin}\" foreground >> \"{log}\" 2>&1\r\n",
        install = install.display(),
        lib = lib.display(),
        bin = bin.display(),
        log = log.display(),
    );

    let changed = if runner.is_file() {
        fs::read_to_string(&runner).unwrap_or_default() != script
    } else {
        true
    };

    fs::write(&runner, script)?;
    Ok(changed)
}

fn task_exists() -> bool {
    Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn create_or_update_task() -> Result<()> {
    let runner = background_runner_path()?;
    if !runner.is_file() {
        bail!("failed to write {}", runner.display());
    }

    let tr = format!("\"{}\"", runner.display());
    let output = Command::new("schtasks")
        .args([
            "/Create",
            "/TN",
            TASK_NAME,
            "/TR",
            &tr,
            "/SC",
            "ONLOGON",
            "/RL",
            "LIMITED",
            "/F",
        ])
        .output()
        .context("failed to run schtasks")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "schtasks failed: {}{}",
            stderr.trim(),
            if stdout.trim().is_empty() {
                String::new()
            } else {
                format!(" ({})", stdout.trim())
            }
        );
    }

    run_task_now()?;
    ensure_tray_task()?;
    run_tray_now()
}

fn ensure_tray_task() -> Result<()> {
    let bin = resolve_agent_binary()?;
    let tr = format!("\"{}\" tray", bin.display());
    let output = Command::new("schtasks")
        .args([
            "/Create",
            "/TN",
            TRAY_TASK_NAME,
            "/TR",
            &tr,
            "/SC",
            "ONLOGON",
            "/RL",
            "LIMITED",
            "/F",
        ])
        .output()
        .context("failed to create tray scheduled task")?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("already exists") || tray_task_exists() {
        return Ok(());
    }

    bail!("failed to register tray task: {}", stderr.trim())
}

fn tray_task_exists() -> bool {
    Command::new("schtasks")
        .args(["/Query", "/TN", TRAY_TASK_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_tray_now() -> Result<()> {
    if !tray_task_exists() {
        return Ok(());
    }
    let _ = Command::new("schtasks")
        .args(["/Run", "/TN", TRAY_TASK_NAME])
        .output();
    Ok(())
}

fn run_task_now() -> Result<()> {
    let output = Command::new("schtasks")
        .args(["/Run", "/TN", TASK_NAME])
        .output()
        .context("failed to start background task")?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("already running") {
        return Ok(());
    }

    bail!("failed to start background agent: {}", stderr.trim());
}
