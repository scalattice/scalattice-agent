use super::github::{download_release_asset, fetch_latest_release};
use super::{compare_versions, current_version, UpdateCheckOutcome, UpdateInfo};
use crate::paths::{install_dir, lib_dir};
use crate::service;
use anyhow::{bail, Context, Result};
use std::cmp::Ordering;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const UPDATE_SERVICE: &str = "scalattice-agent-update.service";
const UPDATE_TIMER: &str = "scalattice-agent-update.timer";

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
        "Downloading Scalattice Agent v{} ({})...",
        info.latest_version,
        linux_archive_name()?
    );
    let staging = download_and_extract(&info.latest_tag).await?;
    println!("Installing update...");
    apply_update(&staging)?;
    println!("Updated to v{}. Background agent restarted.", info.latest_version);
    Ok(())
}

pub fn sync_auto_update_timer(enable: bool) -> Result<()> {
    if !service::background_service_available() {
        if enable {
            bail!("systemd is required to enable automatic updates on Linux");
        }
        return Ok(());
    }

    if enable {
        write_update_units()?;
        run_systemctl(&["--user", "daemon-reload"])?;
        run_systemctl(&["--user", "enable", "--now", UPDATE_TIMER])?;
        println!("Automatic daily updates enabled (systemd timer).");
    } else {
        let _ = run_systemctl(&["--user", "disable", "--now", UPDATE_TIMER]);
        let _ = remove_update_units();
        let _ = run_systemctl(&["--user", "daemon-reload"]);
        println!("Automatic daily updates disabled.");
    }
    Ok(())
}

fn linux_archive_name() -> Result<String> {
    Ok(format!("scalattice-agent-{}.tar.gz", linux_release_target()?))
}

fn linux_release_target() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64-unknown-linux-gnu"),
        "aarch64" => Ok("aarch64-unknown-linux-gnu"),
        other => bail!("automatic updates are not supported on Linux arch: {other}"),
    }
}

async fn download_and_extract(tag: &str) -> Result<PathBuf> {
    let archive_name = linux_archive_name()?;
    let archive_path = update_download_path(tag, &archive_name)?;
    download_release_asset(tag, &archive_name, &archive_path).await?;

    let staging = update_staging_dir(tag)?;
    if staging.exists() {
        fs::remove_dir_all(&staging).ok();
    }
    fs::create_dir_all(&staging).context("create update staging directory")?;
    extract_tarball(&archive_path, &staging)?;
    Ok(staging)
}

fn update_download_path(tag: &str, archive_name: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir().join("scalattice").join("updates");
    let safe_tag = tag.replace('/', "_");
    Ok(base.join(safe_tag).join(archive_name))
}

fn update_staging_dir(tag: &str) -> Result<PathBuf> {
    let safe_tag = tag.replace('/', "_");
    Ok(std::env::temp_dir()
        .join("scalattice")
        .join("updates")
        .join(safe_tag)
        .join("staging"))
}

fn extract_tarball(archive: &Path, dest: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status()
        .context("run tar to extract release archive")?;
    if !status.success() {
        bail!("tar failed extracting {}", archive.display());
    }
    Ok(())
}

fn apply_update(staging: &Path) -> Result<()> {
    let source_bin = staging.join("scalattice-agent");
    if !source_bin.is_file() {
        bail!(
            "release archive did not contain scalattice-agent at {}",
            source_bin.display()
        );
    }

    service::stop_background_for_update()?;

    let install_bin = install_dir().context("resolve install directory")?;
    fs::create_dir_all(&install_bin).context("create install directory")?;
    let dest_bin = install_bin.join("scalattice-agent");
    let tmp_bin = install_bin.join("scalattice-agent.update");
    fs::copy(&source_bin, &tmp_bin).context("copy new agent binary")?;
    fs::set_permissions(&tmp_bin, fs::Permissions::from_mode(0o755))
        .context("set executable bit on new agent binary")?;
    fs::rename(&tmp_bin, &dest_bin).context("replace installed agent binary")?;

    let source_lib = staging.join("lib");
    if source_lib.is_dir() {
        let dest_lib = lib_dir().context("resolve library directory")?;
        fs::create_dir_all(&dest_lib).context("create library directory")?;
        for entry in fs::read_dir(&source_lib).context("read bundled lib directory")? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            fs::copy(entry.path(), dest_lib.join(&name))
                .with_context(|| format!("copy library {}", name.to_string_lossy()))?;
        }
    }

    service::restart_background_after_update()?;
    fs::remove_dir_all(staging.parent().unwrap_or(staging)).ok();
    Ok(())
}

fn write_update_units() -> Result<()> {
    let home = crate::paths::home_dir()?;
    let unit_dir = home.join(".config/systemd/user");
    fs::create_dir_all(&unit_dir).context("create systemd user unit directory")?;

    let bin = crate::paths::resolve_agent_binary().unwrap_or_else(|_| {
        install_dir()
            .map(|d| d.join("scalattice-agent"))
            .unwrap_or_else(|_| PathBuf::from("scalattice-agent"))
    });

    let service = format!(
        "[Unit]\n\
Description=Check for Scalattice Agent updates\n\
\n\
[Service]\n\
Type=oneshot\n\
ExecStart={} update\n",
        bin.display()
    );

    let timer = "[Unit]\n\
Description=Daily Scalattice Agent update check\n\
\n\
[Timer]\n\
OnCalendar=daily\n\
Persistent=true\n\
RandomizedDelaySec=4h\n\
\n\
[Install]\n\
WantedBy=timers.target\n";

    fs::write(unit_dir.join(UPDATE_SERVICE), service)?;
    fs::write(unit_dir.join(UPDATE_TIMER), timer)?;
    Ok(())
}

fn remove_update_units() -> Result<()> {
    let home = crate::paths::home_dir()?;
    let unit_dir = home.join(".config/systemd/user");
    let _ = fs::remove_file(unit_dir.join(UPDATE_SERVICE));
    let _ = fs::remove_file(unit_dir.join(UPDATE_TIMER));
    Ok(())
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
        })
    }
}
