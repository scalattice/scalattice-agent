use super::cloud::{download_release_asset, fetch_latest_release};
use super::{compare_versions, current_version, UpdateCheckOutcome, UpdateInfo};
use anyhow::{Context, Result};
use std::cmp::Ordering;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const INSTALLER_NAME: &str = "ScalatticeAgentSetup-x86_64.exe";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Detach so the setup UI stays up after this process exits.
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

pub async fn check_for_update() -> Result<UpdateCheckOutcome> {
    let latest = fetch_latest_release().await?;
    let current = current_version().to_string();
    let update_available = compare_versions(&latest.version, &current) == Ordering::Greater;
    let info = UpdateInfo {
        current_version: current,
        latest_version: latest.version,
        latest_tag: latest.tag,
        update_available,
    };
    if update_available {
        Ok(UpdateCheckOutcome::UpdateAvailable(info))
    } else {
        Ok(UpdateCheckOutcome::UpToDate(info))
    }
}

pub async fn install_latest_update() -> Result<()> {
    let outcome = check_for_update().await?;
    let info = outcome.info();
    if !info.update_available {
        println!("Already up to date (v{}).", info.current_version);
        return Ok(());
    }

    println!(
        "Downloading Scalattice setup v{}...",
        info.latest_version
    );
    let installer = download_setup(&info.latest_tag).await?;
    println!("Opening Scalattice setup. Finish the installer to complete the update.");
    spawn_setup_and_exit(&installer)?;
    Ok(())
}

async fn download_setup(tag: &str) -> Result<PathBuf> {
    let latest = fetch_latest_release().await?;
    let expected = latest
        .checksums
        .get(INSTALLER_NAME)
        .cloned()
        .with_context(|| {
            format!(
                "Cloud release {tag} has no SHA-256 checksum for {INSTALLER_NAME}; refusing to update"
            )
        })?;
    let dest = update_setup_path(tag)?;
    download_release_asset(tag, INSTALLER_NAME, &dest, &expected).await?;
    Ok(dest)
}

fn update_setup_path(tag: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir().join("Scalattice").join("updates");
    let safe_tag = tag.replace('/', "_");
    Ok(base.join(safe_tag).join(INSTALLER_NAME))
}

/// Launch the official setup wizard, then exit so it can replace running files.
pub fn spawn_setup_and_exit(installer: &Path) -> Result<()> {
    if !installer.is_file() {
        anyhow::bail!("setup missing at {}", installer.display());
    }

    let installer_arg = installer.display().to_string();
    let mut args = vec![
        "/C".to_string(),
        "start".to_string(),
        String::new(), // empty title so `start` does not treat the path as a title
        installer_arg,
    ];
    if let Some(token) = crate::config::read_saved_agent_token() {
        let token = token.trim();
        if !token.is_empty() {
            args.push(format!("/TOKEN={token}"));
        }
    }

    // `cmd /C start "" setup.exe …` detaches a visible setup window reliably.
    Command::new("cmd.exe")
        .args(&args)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("launch setup {}", installer.display()))?;

    // Exit this process (CLI or tray). Setup closes any remaining agent processes itself.
    std::process::exit(0);
}
