use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIT_NAME: &str = "scalattice-agent.service";

pub fn ensure_service_running() -> Result<()> {
    if !systemd_available() {
        bail!("systemd is required for background mode — use: scalattice-agent connect --foreground");
    }

    let home = home_dir()?;
    let unit_path = systemd_user_unit_path(&home);

    if !unit_path.is_file() {
        install_user_service()?;
    } else {
        sync_systemd_env_file(&home)?;
        run_systemctl(&["--user", "daemon-reload"])?;
        if run_systemctl(&["--user", "is-active", UNIT_NAME]).is_err() {
            run_systemctl(&["--user", "start", UNIT_NAME])?;
        } else {
            println!("scalattice-agent is running in the background");
            return Ok(());
        }
    }

    verify_service_active()
}

pub fn install_user_service() -> Result<()> {
    let home = home_dir()?;
    let unit_path = systemd_user_unit_path(&home);
    let env_file = home.join(".config/scalattice/agent.env");
    let bin = resolve_agent_binary()?;

    if !env_file.is_file() {
        bail!(
            "missing {} — run the install script or create agent.env with SCALATTICE_AGENT_TOKEN",
            env_file.display()
        );
    }

    sync_systemd_env_file(&home)?;

    let systemd_env = systemd_env_path(&home);
    let path_prefix = format!("{}:/usr/local/bin:/usr/bin:/bin", bin.parent().unwrap_or(Path::new("/usr/local/bin")).display());

    fs::create_dir_all(unit_path.parent().context("unit path parent")?)?;
    let unit = format!(
        r#"[Unit]
Description=Scalattice GPU Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
Environment=PATH={path}
EnvironmentFile={env}
ExecStart={bin} connect --foreground
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"#,
        path = path_prefix,
        env = systemd_env.display(),
        bin = bin.display(),
    );
    fs::write(&unit_path, unit)?;
    println!("Wrote {}", unit_path.display());

    run_systemctl(&["--user", "daemon-reload"])?;
    run_systemctl(&["--user", "enable", "--now", UNIT_NAME])?;
    verify_service_active()?;
    enable_linger_hint(&home);
    Ok(())
}

pub fn uninstall_user_service() -> Result<()> {
    let home = home_dir()?;
    let unit_path = systemd_user_unit_path(&home);

    let _ = run_systemctl(&["--user", "disable", "--now", UNIT_NAME]);
    if unit_path.is_file() {
        fs::remove_file(&unit_path)?;
        println!("Removed {}", unit_path.display());
    }
    let _ = run_systemctl(&["--user", "daemon-reload"]);
    Ok(())
}

pub fn service_status() -> Result<()> {
    if !systemd_available() {
        println!("systemd: not available on this system");
        return Ok(());
    }

    let home = home_dir()?;
    let unit_path = systemd_user_unit_path(&home);
    if unit_path.is_file() {
        println!("unit file: {}", unit_path.display());
    } else {
        println!("unit file: not installed (run: scalattice-agent connect)");
    }

    if run_systemctl(&["--user", "is-active", UNIT_NAME]).is_ok() {
        println!("service: active");
    } else {
        println!("service: not running");
    }

    if run_systemctl(&["--user", "is-enabled", UNIT_NAME]).is_ok() {
        println!("boot: enabled (starts after login unless lingering is on)");
    } else {
        println!("boot: disabled");
    }

    Ok(())
}

fn verify_service_active() -> Result<()> {
    if run_systemctl(&["--user", "is-active", UNIT_NAME]).is_ok() {
        println!("scalattice-agent is running in the background");
        return Ok(());
    }

    eprintln!("service failed to start — recent logs:");
    let _ = Command::new("systemctl")
        .args(["--user", "status", UNIT_NAME, "--no-pager", "-n", "15"])
        .status();

    bail!("scalattice-agent service is not running — try: scalattice-agent connect --foreground");
}

fn sync_systemd_env_file(home: &Path) -> Result<()> {
    let env_file = home.join(".config/scalattice/agent.env");
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

    fs::create_dir_all(systemd_env.parent().context("systemd env parent")?)?;
    fs::write(&systemd_env, format!("{}\n", lines.join("\n")))?;
    Ok(())
}

fn systemd_env_path(home: &Path) -> PathBuf {
    home.join(".config/scalattice/agent.systemd.env")
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn systemd_user_unit_path(home: &Path) -> PathBuf {
    home.join(".config/systemd/user").join(UNIT_NAME)
}

fn resolve_agent_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("SCALATTICE_AGENT_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }

    if let Ok(path) = which::which("scalattice-agent") {
        return Ok(path);
    }

    let local = home_dir()?.join(".local/bin/scalattice-agent");
    if local.is_file() {
        return Ok(local);
    }

    bail!("scalattice-agent binary not found in PATH or ~/.local/bin");
}

pub fn systemd_available() -> bool {
    Command::new("systemctl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .context("failed to run systemctl")?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !stdout.is_empty() {
            println!("{stdout}");
        }
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

fn enable_linger_hint(home: &Path) {
    let user = home
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("YOUR_USER");
    println!();
    println!("To start automatically after reboot (without logging in):");
    println!("  sudo loginctl enable-linger {user}");
}

mod which {
    use std::path::PathBuf;
    use std::process::Command;

    pub fn which(name: &str) -> Result<PathBuf, ()> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {name}"))
            .output()
            .map_err(|_| ())?;
        if !output.status.success() {
            return Err(());
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() {
            return Err(());
        }
        Ok(PathBuf::from(path))
    }
}
