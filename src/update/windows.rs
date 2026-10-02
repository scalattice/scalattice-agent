use super::cloud::{download_release_asset, fetch_latest_release};
use super::{compare_versions, current_version, UpdateCheckOutcome, UpdateInfo};
use anyhow::{Context, Result};
use std::cmp::Ordering;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const INSTALLER_NAME: &str = "ScalatticeAgentSetup-x86_64.exe";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Detach so setup keeps running after this process exits.
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

pub async fn check_for_update() -> Result<UpdateCheckOutcome> {
    let latest = fetch_latest_release().await?;
    let current = current_version().to_string();
    // Cloud `/latest` is the served channel tip (safeAgentVersion pin or GitHub
    // tip). Install when we are not on that tip — including rollbacks.
    let update_available = compare_versions(&latest.version, &current) != Ordering::Equal;
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
    let latest = fetch_latest_release().await?;
    let current = current_version().to_string();
    let update_available = compare_versions(&latest.version, &current) != Ordering::Equal;
    if !update_available {
        println!("Already on channel tip (v{current}).");
        return Ok(());
    }
    let latest_version = latest.version.clone();
    let latest_tag = latest.tag.clone();

    let need = super::disk_need_for_asset(&latest, INSTALLER_NAME).await;
    // Model caches often fill the disk; free space before download/extract.
    super::ensure_disk_for_update(need).await?;

    println!("Downloading Scalattice setup v{latest_version}...");
    let installer = download_setup(&latest_tag, &latest).await?;
    super::wait_until_safe_to_apply().await?;
    println!("Installing update silently in the background…");
    // Drain flag is cleared by the new process; clear best-effort before exit.
    crate::state::end_update_drain();
    spawn_silent_setup_and_exit(&installer)?;
    Ok(())
}

async fn download_setup(tag: &str, latest: &super::cloud::LatestRelease) -> Result<PathBuf> {
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
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    Ok(base.join(safe_tag).join(unique).join(INSTALLER_NAME))
}

/// Launch Inno Setup with no UI. The ISS already handles `/UPDATE=1` and `WizardSilent`:
/// skip device/token pages, replace files, then `scalattice-agent restart`.
pub fn spawn_silent_setup_and_exit(installer: &Path) -> Result<()> {
    if !installer.is_file() {
        anyhow::bail!("setup missing at {}", installer.display());
    }

    let mut args = vec![
        "/VERYSILENT".to_string(),
        "/SUPPRESSMSGBOXES".to_string(),
        "/NORESTART".to_string(),
        "/UPDATE=1".to_string(),
    ];
    // Never /CLOSEAPPLICATIONS during CI: that taskkills the host runner's
    // production agent (same ImageName). Isolated /DIR= is enough.
    if crate::config::update_smoke_test() {
        args.push("/NOCLOSEAPPLICATIONS".to_string());
    } else {
        args.push("/CLOSEAPPLICATIONS".to_string());
    }
    // Pin the existing install dir so UsePreviousAppDir cannot redirect a
    // silent update onto another copy (CI isolate, or a leftover AppId).
    if let Ok(dir) = crate::paths::install_dir() {
        args.push(format!("/DIR={}", dir.display()));
    }
    if let Some(token) = crate::config::read_saved_agent_token() {
        let token = token.trim();
        if !token.is_empty() {
            args.push(format!("/TOKEN={token}"));
        }
    }

    let mut cmd = Command::new(installer);
    cmd.args(&args);
    if let Ok(dir) = crate::paths::install_dir() {
        cmd.env("SCALATTICE_INSTALL_DIR", &dir);
    }
    if let Ok(dir) = crate::paths::lib_dir() {
        cmd.env("SCALATTICE_LIB_DIR", dir);
    }
    cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("launch silent setup {}", installer.display()))?;

    // Exit this process (CLI or tray) so setup can replace locked files.
    std::process::exit(0);
}
