use super::github::{download_release_asset, fetch_latest_release};
use super::{compare_versions, current_version, UpdateCheckOutcome, UpdateInfo};
use crate::paths::install_dir;
use anyhow::{Context, Result};
use std::cmp::Ordering;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const INSTALLER_NAME: &str = "ScalatticeAgentSetup-x86_64.exe";

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
        "Downloading Scalattice Agent v{}...",
        info.latest_version
    );
    let installer = download_installer(&info.latest_tag).await?;
    println!("Installing update (the tray will restart automatically)...");
    spawn_installer_and_exit(&installer)?;
    Ok(())
}

async fn download_installer(tag: &str) -> Result<PathBuf> {
    let dest = update_installer_path(tag)?;
    download_release_asset(tag, INSTALLER_NAME, &dest).await?;
    Ok(dest)
}

fn update_installer_path(tag: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir().join("Scalattice").join("updates");
    let safe_tag = tag.replace('/', "_");
    Ok(base.join(safe_tag).join(INSTALLER_NAME))
}

pub fn spawn_installer_and_exit(installer: &Path) -> Result<()> {
    let install = install_dir().context("resolve install directory")?;
    let runner = write_update_runner(installer, &install)?;
    Command::new("cmd.exe")
        .args(["/C", "start", "", "/MIN", runner.to_string_lossy().as_ref()])
        .creation_flags(0x0800_0000)
        .spawn()
        .context("launch update runner")?;
    std::process::exit(0);
}

fn write_update_runner(installer: &Path, install_dir: &Path) -> Result<PathBuf> {
    let runner = std::env::temp_dir()
        .join("Scalattice")
        .join("scalattice-update.cmd");
    if let Some(parent) = runner.parent() {
        fs::create_dir_all(parent)?;
    }

    let installer = installer.display();
    let install = install_dir.display();
    let script = format!(
        "@echo off\r\n\
setlocal\r\n\
timeout /t 2 /nobreak >nul\r\n\
powershell -NoProfile -Command \"Get-CimInstance Win32_Process -Filter \\\"name='scalattice-agent.exe'\\\" | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}\"\r\n\
\"{installer}\" /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS /UPDATE=1\r\n\
if exist \"{install}\\launch-background.vbs\" wscript.exe //nologo \"{install}\\launch-background.vbs\"\r\n\
if exist \"{install}\\launch-tray.vbs\" wscript.exe //nologo \"{install}\\launch-tray.vbs\"\r\n\
del /f /q \"%~f0\" >nul 2>&1\r\n"
    );

    fs::write(&runner, script).with_context(|| format!("write {}", runner.display()))?;
    Ok(runner)
}
