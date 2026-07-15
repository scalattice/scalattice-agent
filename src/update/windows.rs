use super::cloud::{download_release_asset, fetch_latest_release};
use super::{compare_versions, current_version, UpdateCheckOutcome, UpdateInfo};
use crate::paths::install_dir;
use anyhow::{Context, Result};
use std::cmp::Ordering;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const INSTALLER_NAME: &str = "ScalatticeAgentSetup-x86_64.exe";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
    let dest = update_installer_path(tag)?;
    download_release_asset(tag, INSTALLER_NAME, &dest, &expected).await?;
    Ok(dest)
}

fn update_installer_path(tag: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir().join("Scalattice").join("updates");
    let safe_tag = tag.replace('/', "_");
    Ok(base.join(safe_tag).join(INSTALLER_NAME))
}

pub fn spawn_installer_and_exit(installer: &Path) -> Result<()> {
    let install = install_dir().context("resolve install directory")?;
    let runner = write_update_runner_ps1(installer, &install)?;
    if !runner.is_file() {
        anyhow::bail!("update runner script missing at {}", runner.display());
    }

    // `start` detaches the updater so it survives this process exiting.
    // Empty title string is required by cmd's start parsing.
    let status = Command::new("cmd.exe")
        .args([
            "/C",
            "start",
            "ScalatticeUpdate",
            "/MIN",
            "powershell.exe",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
            &runner.display().to_string(),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("launch update runner {}", runner.display()))?;

    if !status.success() {
        anyhow::bail!("failed to detach Windows update runner");
    }

    std::process::exit(0);
}

fn write_update_runner_ps1(installer: &Path, install_dir: &Path) -> Result<PathBuf> {
    let runner = std::env::temp_dir()
        .join("Scalattice")
        .join("scalattice-update.ps1");
    if let Some(parent) = runner.parent() {
        fs::create_dir_all(parent)?;
    }

    let installer = installer.display().to_string().replace('\'', "''");
    let install = install_dir.display().to_string().replace('\'', "''");
    let script = format!(
        r#"$ErrorActionPreference = 'Continue'
$logDir = Join-Path $env:LOCALAPPDATA 'Scalattice\logs'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$log = Join-Path $logDir 'update.log'
function Log([string]$msg) {{
  $line = '{{0}} {{1}}' -f (Get-Date -Format o), $msg
  Add-Content -LiteralPath $log -Value $line -ErrorAction SilentlyContinue
}}
Log 'update runner start'
Start-Sleep -Seconds 3
Get-CimInstance Win32_Process -Filter "name='scalattice-agent.exe'" -ErrorAction SilentlyContinue | ForEach-Object {{
  Log ("stopping pid " + $_.ProcessId)
  Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
}}
$deadline = (Get-Date).AddSeconds(45)
while ((Get-Date) -lt $deadline) {{
  $left = @(Get-CimInstance Win32_Process -Filter "name='scalattice-agent.exe'" -ErrorAction SilentlyContinue)
  if ($left.Count -eq 0) {{ break }}
  $left | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}
  Start-Sleep -Milliseconds 500
}}
Start-Sleep -Seconds 2
Log 'starting installer'
$p = Start-Process -FilePath '{installer}' -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/CLOSEAPPLICATIONS','/FORCECLOSEAPPLICATIONS','/UPDATE=1' -Wait -PassThru -WindowStyle Hidden
$code = 0
if ($null -ne $p) {{ $code = $p.ExitCode }}
Log ("installer exit code " + $code)
# 0 = success, 3010 = success reboot required (we pass /NORESTART)
if (($code -ne 0) -and ($code -ne 3010)) {{
  Log 'installer failed; not launching agent'
  exit 1
}}
$bin = Join-Path '{install}' 'scalattice-agent.exe'
$bg = Join-Path '{install}' 'launch-background.vbs'
$tray = Join-Path '{install}' 'launch-tray.vbs'
if (Test-Path -LiteralPath $bin) {{
  try {{
    $ver = (Get-Item -LiteralPath $bin).VersionInfo.ProductVersion
    Log ("installed binary ProductVersion=" + $ver)
  }} catch {{}}
  Log 'running scalattice-agent restart'
  $r = Start-Process -FilePath $bin -ArgumentList 'restart' -Wait -PassThru -WindowStyle Hidden
  if ($null -ne $r) {{ Log ("restart exit code " + $r.ExitCode) }}
}}
Start-Sleep -Seconds 2
if (Test-Path -LiteralPath $bg) {{
  Log 'launch-background.vbs'
  Start-Process -FilePath 'wscript.exe' -ArgumentList '//nologo', $bg -WindowStyle Hidden
}}
Start-Sleep -Seconds 1
if (Test-Path -LiteralPath $tray) {{
  Log 'launch-tray.vbs'
  Start-Process -FilePath 'wscript.exe' -ArgumentList '//nologo', $tray -WindowStyle Hidden
}}
$alive = @(Get-CimInstance Win32_Process -Filter "name='scalattice-agent.exe'" -ErrorAction SilentlyContinue)
Log ("agent processes after update: " + $alive.Count)
Remove-Item -LiteralPath $MyInvocation.MyCommand.Path -Force -ErrorAction SilentlyContinue
Log 'update runner done'
"#
    );

    // Remove any legacy .cmd updater that flashed a console.
    let legacy_cmd = std::env::temp_dir()
        .join("Scalattice")
        .join("scalattice-update.cmd");
    let _ = fs::remove_file(legacy_cmd);

    fs::write(&runner, script).with_context(|| format!("write {}", runner.display()))?;
    Ok(runner)
}
