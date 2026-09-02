use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub fn home_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SCALATTICE_HOME") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    os_user_home()
}

/// Login-session home used for systemd user units (Linux). Independent of
/// `SCALATTICE_HOME`, which isolates agent data for the CI update smoke.
pub fn os_user_home() -> Result<PathBuf> {
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

pub fn settings_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("settings.json"))
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
        Ok(home_dir()?
            .join(".local")
            .join("share")
            .join("scalattice")
            .join("agent.log"))
    }
}

pub fn agent_binary_name() -> &'static str {
    if cfg!(windows) {
        "scalattice-agent.exe"
    } else {
        "scalattice-agent"
    }
}

#[cfg(target_os = "macos")]
pub fn bundled_macos_agent_binary() -> PathBuf {
    PathBuf::from("/Applications/Scalattice Agent.app/Contents/MacOS/scalattice-agent")
}

#[cfg(target_os = "macos")]
fn looks_like_agent_binary(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == agent_binary_name() || n == "scalattice-agent")
        .unwrap_or(false)
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn paths_eq(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Locations that a Unix self-update should replace.
///
/// macOS ships as an .app bundle; `install_dir()` is `~/.local/bin`. Replacing
/// only the latter leaves launchd running the old bundle binary.
#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
pub fn unix_agent_install_targets() -> Result<Vec<PathBuf>> {
    let mut targets: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if path.as_os_str().is_empty() {
            return;
        }
        if targets.iter().any(|existing| paths_eq(existing, &path)) {
            return;
        }
        targets.push(path);
    };

    #[cfg(target_os = "macos")]
    {
        if let Ok(exe) = std::env::current_exe() {
            if looks_like_agent_binary(&exe) {
                push(exe);
            }
        }
        let app = bundled_macos_agent_binary();
        if app.is_file() {
            push(app);
        }
    }

    push(install_dir()?.join(agent_binary_name()));
    Ok(targets)
}

pub fn resolve_agent_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("SCALATTICE_AGENT_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(exe) = std::env::current_exe() {
            if exe.is_file() && looks_like_agent_binary(&exe) {
                return Ok(exe);
            }
        }
        let app = bundled_macos_agent_binary();
        if app.is_file() {
            return Ok(app);
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

/// Ensure bundled CUDA/Vulkan DLLs resolve when the exe is launched from a Start Menu shortcut.
#[cfg(windows)]
pub fn init_windows_native_search_path() {
    let Ok(install) = install_dir() else {
        return;
    };
    if !install.is_dir() {
        return;
    }
    let lib = lib_dir().ok().filter(|path| path.is_dir());
    let current = std::env::var("PATH").unwrap_or_default();
    let mut prefix = install.display().to_string();
    if let Some(lib) = &lib {
        prefix = format!("{};{}", prefix, lib.display());
    }
    if current
        .to_ascii_lowercase()
        .starts_with(&prefix.to_ascii_lowercase())
    {
        return;
    }
    let _ = std::env::set_var("PATH", format!("{prefix};{current}"));
}

mod which {
    use std::path::PathBuf;
    use std::process::Command;

    pub fn which(name: &str) -> Result<PathBuf, ()> {
        #[cfg(windows)]
        {
            let mut command = Command::new("where");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                command.creation_flags(CREATE_NO_WINDOW);
            }
            let output = command.arg(name).output().map_err(|_| ())?;
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

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn unix_install_targets_include_local_bin() {
        let targets = unix_agent_install_targets().expect("targets");
        let expected = install_dir()
            .expect("install dir")
            .join(agent_binary_name());
        assert!(
            targets
                .iter()
                .any(|path| path == &expected || paths_eq(path, &expected)),
            "expected {} in {targets:?}",
            expected.display()
        );
    }
}
