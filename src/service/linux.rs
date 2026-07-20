use crate::config::AgentConfig;
use crate::paths::resolve_agent_binary;
use crate::service::BackgroundStatus;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIT_NAME: &str = "scalattice-agent.service";

pub fn background_status() -> BackgroundStatus {
    if !background_service_available() {
        return BackgroundStatus::NotInstalled;
    }
    let home = match crate::paths::home_dir() {
        Ok(h) => h,
        Err(_) => return BackgroundStatus::NotInstalled,
    };
    if !systemd_user_unit_path(&home).is_file() {
        return BackgroundStatus::NotInstalled;
    }
    if service_active() {
        BackgroundStatus::Running
    } else {
        BackgroundStatus::Stopped
    }
}

pub fn start_background_from_config(config: &AgentConfig) -> Result<()> {
    ensure_service_running(config)
}

pub fn restart_after_token_change(config: &AgentConfig) -> Result<()> {
    restart_background_from_config(config)
}

pub fn restart_background_from_config(config: &AgentConfig) -> Result<()> {
    if !background_service_available() {
        return ensure_service_running(config);
    }

    let home = crate::paths::home_dir()?;
    let unit_path = systemd_user_unit_path(&home);
    let _ = crate::service::persist_agent_token(&config.token)?;
    let _ = write_user_unit(&home)?;
    sync_systemd_env_file(&home)?;
    run_systemctl(&["--user", "daemon-reload"])?;

    if unit_path.is_file() {
        run_systemctl(&["--user", "restart", UNIT_NAME])?;
    } else {
        run_systemctl(&["--user", "enable", "--now", UNIT_NAME])?;
    }

    verify_service_active()?;
    Ok(())
}

pub fn invoked_by_systemd() -> bool {
    std::env::var("INVOCATION_ID").is_ok() || std::env::var("JOURNAL_STREAM").is_ok()
}

pub fn invoked_by_background_service() -> bool {
    false
}

pub fn background_service_available() -> bool {
    Command::new("systemctl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn service_active() -> bool {
    if !background_service_available() {
        return false;
    }
    systemctl_success(&["--user", "is-active", UNIT_NAME])
}

pub fn follow_service_logs(verbose: bool) -> Result<()> {
    if !background_service_available() {
        anyhow::bail!("systemd is not available on this system");
    }
    if !service_active() {
        anyhow::bail!(
            "scalattice-agent is not running; save your token with `scalattice-agent set-token` first"
        );
    }

    let mut child = Command::new("journalctl")
        .args([
            "--user",
            "-f",
            "-u",
            UNIT_NAME,
            "-n",
            "30",
            "--no-pager",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("failed to run journalctl")?;

    let stdout = child
        .stdout
        .take()
        .context("journalctl stdout missing")?;
    crate::logging::pipe_log_lines(stdout, verbose)?;

    match child.wait().context("wait for journalctl")?.code() {
        Some(130) | Some(0) => Ok(()),
        Some(code) => anyhow::bail!("journalctl exited with status {code}"),
        None => anyhow::bail!("journalctl terminated by signal"),
    }
}

pub fn sync_background_env() -> Result<()> {
    let home = crate::paths::home_dir()?;
    sync_systemd_env_file(&home)
}

pub fn remove_background_service() -> Result<()> {
    uninstall_user_service()
}

pub fn stop_background_for_update() -> Result<()> {
    if !background_service_available() || !service_active() {
        return Ok(());
    }
    run_systemctl(&["--user", "stop", UNIT_NAME])?;
    Ok(())
}

pub fn restart_background_after_update() -> Result<()> {
    if !background_service_available() {
        return Ok(());
    }
    let home = crate::paths::home_dir()?;
    if !systemd_user_unit_path(&home).is_file() {
        return Ok(());
    }
    run_systemctl(&["--user", "restart", UNIT_NAME])?;
    Ok(())
}

pub fn systemd_unit_path() -> Result<PathBuf> {
    Ok(systemd_user_unit_path(&crate::paths::home_dir()?))
}

fn ensure_service_running(config: &AgentConfig) -> Result<()> {
    if !background_service_available() {
        bail!("systemd is required for background mode - use: scalattice-agent foreground");
    }

    let home = crate::paths::home_dir()?;
    let unit_path = systemd_user_unit_path(&home);
    let token_changed = crate::service::persist_agent_token(&config.token)?;
    let unit_changed = write_user_unit(&home)?;

    if !unit_path.is_file() {
        bail!("failed to write {}", unit_path.display());
    }

    sync_systemd_env_file(&home)?;
    run_systemctl(&["--user", "daemon-reload"])?;

    let was_active = run_systemctl(&["--user", "is-active", UNIT_NAME]).is_ok();

    if token_changed || unit_changed {
        if was_active {
            let _ = run_systemctl(&["--user", "restart", UNIT_NAME]);
        } else {
            run_systemctl(&["--user", "enable", "--now", UNIT_NAME])?;
        }
    } else if !was_active {
        run_systemctl(&["--user", "enable", "--now", UNIT_NAME])?;
    } else {
        return Ok(());
    }

    verify_service_active()?;
    if unit_changed {
        try_enable_linger(&home);
    }
    Ok(())
}

fn write_user_unit(home: &Path) -> Result<bool> {
    let unit_path = systemd_user_unit_path(home);
    let bin = resolve_agent_binary()?;
    let systemd_env = systemd_env_path(home);
    let path_prefix = format!(
        "{}:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        bin.parent().unwrap_or(Path::new("/usr/local/bin")).display()
    );

    let unit = format!(
        r#"[Unit]
Description=Scalattice GPU Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
Environment=PATH={path}
EnvironmentFile={env}
ExecStart={bin} foreground
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"#,
        path = path_prefix,
        env = systemd_env.display(),
        bin = bin.display(),
    );

    let changed = if unit_path.is_file() {
        let existing = fs::read_to_string(&unit_path).unwrap_or_default();
        existing != unit
    } else {
        true
    };

    fs::create_dir_all(unit_path.parent().context("unit path parent")?)?;
    fs::write(&unit_path, unit)?;
    Ok(changed)
}

fn uninstall_user_service() -> Result<()> {
    let home = crate::paths::home_dir()?;
    let unit_path = systemd_user_unit_path(&home);

    let _ = run_systemctl(&["--user", "disable", "--now", UNIT_NAME]);
    if unit_path.is_file() {
        fs::remove_file(&unit_path)?;
        println!("Removed {}", unit_path.display());
    }
    let _ = run_systemctl(&["--user", "daemon-reload"]);
    Ok(())
}

fn sync_systemd_env_file(home: &Path) -> Result<()> {
    let env_file = crate::paths::agent_env_path()?;
    let systemd_env = systemd_env_path(home);
    let raw = fs::read_to_string(&env_file)
        .with_context(|| format!("read {}", env_file.display()))?;

    let mut lines = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let assignment = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        if assignment.contains('=') {
            lines.push(assignment.to_string());
        }
    }

    if lines.is_empty() {
        bail!("no variables found in {}", env_file.display());
    }

    append_wsl_ld_library_path(&mut lines);

    fs::create_dir_all(systemd_env.parent().context("systemd env parent")?)?;
    fs::write(&systemd_env, format!("{}\n", lines.join("\n")))?;
    Ok(())
}

fn systemd_env_path(home: &Path) -> PathBuf {
    home.join(".config/scalattice/agent.systemd.env")
}

fn systemd_user_unit_path(home: &Path) -> PathBuf {
    home.join(".config/systemd/user").join(UNIT_NAME)
}

fn append_wsl_ld_library_path(lines: &mut Vec<String>) {
    const WSL_LIB: &str = "/usr/lib/wsl/lib";
    if !Path::new(WSL_LIB).is_dir() {
        return;
    }

    for line in lines.iter_mut() {
        if let Some(value) = line.strip_prefix("LD_LIBRARY_PATH=") {
            if value.split(':').any(|part| part == WSL_LIB) {
                return;
            }
            *line = format!("LD_LIBRARY_PATH={WSL_LIB}:{value}");
            return;
        }
    }

    lines.push(format!("LD_LIBRARY_PATH={WSL_LIB}"));
}

fn systemctl_success(args: &[&str]) -> bool {
    Command::new("systemctl")
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn verify_service_active() -> Result<()> {
    if systemctl_success(&["--user", "is-active", UNIT_NAME]) {
        return Ok(());
    }

    eprintln!("service failed to start - recent logs:");
    let _ = Command::new("systemctl")
        .args(["--user", "status", UNIT_NAME, "--no-pager", "-n", "15"])
        .status();

    bail!("scalattice-agent is not running - try: scalattice-agent set-token --token …");
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .context("failed to run systemctl")?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(if stderr.is_empty() {
            format!("systemctl {} failed", args.join(" "))
        } else {
            stderr
        });
    }
}

fn try_enable_linger(home: &Path) -> bool {
    let user = home
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    if user.is_empty() {
        return false;
    }

    let already = Command::new("loginctl")
        .args(["show-user", user, "-p", "Linger", "--value"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
        .unwrap_or(false);

    if already {
        println!("==> Boot without login: already enabled");
        return true;
    }

    if Command::new("sudo")
        .args(["-n", "loginctl", "enable-linger", user])
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        println!("==> Boot without login: enabled");
        return true;
    }

    println!("==> Boot without login: needs sudo - run once:");
    println!("    sudo loginctl enable-linger {user}");
    false
}
