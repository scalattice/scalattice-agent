use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub fn home_dir() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        return std::env::var("HOME")
            .map(PathBuf::from)
            .context("HOME is not set");
    }
    #[cfg(windows)]
    {
        return std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .context("USERPROFILE is not set");
    }
}

pub fn config_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config").join("scalattice"))
}

pub fn agent_env_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agent.env"))
}

pub fn agent_state_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agent.state.json"))
}

pub fn install_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SCALATTICE_INSTALL_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    #[cfg(windows)]
    {
        let base = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|_| home_dir())?;
        return Ok(base.join("Scalattice").join("bin"));
    }

    #[cfg(unix)]
    {
        Ok(home_dir()?.join(".local").join("bin"))
    }
}

pub fn lib_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SCALATTICE_LIB_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    #[cfg(windows)]
    {
        let base = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|_| home_dir())?;
        return Ok(base.join("Scalattice").join("lib"));
    }

    #[cfg(unix)]
    {
        Ok(home_dir()?.join(".local").join("lib").join("scalattice"))
    }
}

pub fn models_cache_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".cache")
        .join("scalattice")
        .join("models")
}

pub fn agent_log_path() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|_| home_dir())?;
        return Ok(base.join("Scalattice").join("logs").join("agent.log"));
    }

    #[cfg(unix)]
    {
        Ok(home_dir()?.join(".local").join("share").join("scalattice").join("agent.log"))
    }
}

pub fn agent_binary_name() -> &'static str {
    if cfg!(windows) {
        "scalattice-agent.exe"
    } else {
        "scalattice-agent"
    }
}

pub fn resolve_agent_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("SCALATTICE_AGENT_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }

    if let Ok(path) = which::which(agent_binary_name()) {
        return Ok(path);
    }

    let local = install_dir()?.join(agent_binary_name());
    if local.is_file() {
        return Ok(local);
    }

    anyhow::bail!(
        "{} binary not found in PATH or install dir",
        agent_binary_name()
    );
}

mod which {
    use std::path::PathBuf;
    use std::process::Command;

    pub fn which(name: &str) -> Result<PathBuf, ()> {
        #[cfg(windows)]
        {
            let output = Command::new("where")
                .arg(name)
                .output()
                .map_err(|_| ())?;
            if !output.status.success() {
                return Err(());
            }
            let path = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if path.is_empty() {
                return Err(());
            }
            return Ok(PathBuf::from(path));
        }

        #[cfg(unix)]
        {
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
}

pub fn is_dir_empty(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

pub fn remove_path_quiet(path: &Path) {
    if path.is_dir() {
        match std::fs::remove_dir_all(path) {
            Ok(()) => println!("Removed {}", path.display()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => eprintln!("Warning: could not remove {}: {err}", path.display()),
        }
        return;
    }

    match std::fs::remove_file(path) {
        Ok(()) => println!("Removed {}", path.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => eprintln!("Warning: could not remove {}: {err}", path.display()),
    }
}
