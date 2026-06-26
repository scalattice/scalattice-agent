use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const UNIT_NAME: &str = "scalattice-agent.service";

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

    fs::create_dir_all(unit_path.parent().context("unit path parent")?)?;
    let unit = format!(
        r#"[Unit]
Description=Scalattice GPU Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile={env}
ExecStart={bin} connect
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"#,
        env = env_file.display(),
        bin = bin.display(),
    );
    fs::write(&unit_path, unit)?;
    println!("Wrote {}", unit_path.display());

    run_systemctl(&["--user", "daemon-reload"])?;
    run_systemctl(&["--user", "enable", "--now", UNIT_NAME])?;
    enable_linger_hint(&home);

    println!("Service enabled. Check: scalattice-agent service status");
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
        println!("unit file: not installed (run: scalattice-agent service install)");
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

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn systemd_user_unit_path(home: &PathBuf) -> PathBuf {
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

fn systemd_available() -> bool {
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

fn enable_linger_hint(home: &PathBuf) {
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
