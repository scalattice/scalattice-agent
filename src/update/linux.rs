use super::cloud::{download_release_asset, fetch_latest_release};
use super::{compare_versions, current_version, UpdateCheckOutcome, UpdateInfo};
use crate::paths::{lib_dir, unix_agent_install_targets};
use crate::service;
use anyhow::{bail, Context, Result};
use std::cmp::Ordering;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "linux")]
const UPDATE_SERVICE: &str = "scalattice-agent-update.service";
#[cfg(target_os = "linux")]
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

    if !running_as_live_agent() {
        // Stop the live agent before the (long) download so a dashboard remote
        // update cannot clobber the same /tmp archive while this CLI runs.
        service::stop_background_for_update()?;
    }

    println!(
        "Downloading Scalattice Agent v{} ({})...",
        info.latest_version,
        unix_archive_name()?
    );
    let staging = download_and_extract(&info.latest_tag).await?;
    println!("Installing update...");
    apply_update(&staging)?;
    if running_as_live_agent() {
        println!(
            "Updated to v{}. Live agent will restart onto the new binary.",
            info.latest_version
        );
    } else {
        println!(
            "Updated to v{}. Background agent restarted.",
            info.latest_version
        );
    }
    Ok(())
}

pub fn sync_auto_update_timer(enable: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        return crate::service::sync_macos_auto_update(enable);
    }
    #[cfg(target_os = "linux")]
    {
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
}

fn unix_archive_name() -> Result<String> {
    Ok(format!("scalattice-agent-{}.tar.gz", unix_release_target()?))
}

fn unix_release_target() -> Result<&'static str> {
    #[cfg(target_os = "macos")]
    {
        if std::env::consts::ARCH != "aarch64" {
            bail!("automatic updates support Apple Silicon only");
        }
        return Ok("aarch64-apple-darwin");
    }
    #[cfg(target_os = "linux")]
    {
        return match std::env::consts::ARCH {
            "x86_64" => Ok("x86_64-unknown-linux-gnu"),
            "aarch64" => Ok("aarch64-unknown-linux-gnu"),
            other => bail!("automatic updates are not supported on Linux arch: {other}"),
        };
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        bail!("automatic updates are not supported on this platform")
    }
}

async fn download_and_extract(tag: &str) -> Result<PathBuf> {
    let latest = fetch_latest_release().await?;
    let archive_name = unix_archive_name()?;
    let expected = latest
        .checksums
        .get(&archive_name)
        .cloned()
        .with_context(|| {
            format!("Cloud release {tag} has no SHA-256 checksum for {archive_name}; refusing to update")
        })?;
    let work = update_work_dir(tag)?;
    fs::create_dir_all(&work).context("create update work directory")?;
    let archive_path = work.join(&archive_name);
    download_release_asset(tag, &archive_name, &archive_path, &expected).await?;

    let staging = work.join("staging");
    fs::create_dir_all(&staging).context("create update staging directory")?;
    if let Err(err) = extract_tarball(&archive_path, &staging) {
        let _ = fs::remove_dir_all(&work);
        return Err(err);
    }
    Ok(staging)
}

fn update_work_dir(tag: &str) -> Result<PathBuf> {
    let safe_tag = tag.replace('/', "_");
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    Ok(std::env::temp_dir()
        .join("scalattice")
        .join("updates")
        .join(safe_tag)
        .join(unique))
}

fn extract_tarball(archive: &Path, dest: &Path) -> Result<()> {
    // Prefer in-process extract: systemd user units often run with a PATH that
    // does not include `tar` (ENOENT → "run tar to extract… No such file or directory").
    match extract_tarball_rust(archive, dest) {
        Ok(()) => return Ok(()),
        Err(rust_err) => {
            if let Some(tar_bin) = resolve_tar_binary() {
                let status = Command::new(&tar_bin)
                    .arg("-xzf")
                    .arg(archive)
                    .arg("-C")
                    .arg(dest)
                    .status()
                    .with_context(|| {
                        format!(
                            "run {} to extract release archive (rust extract also failed: {rust_err:#})",
                            tar_bin.display()
                        )
                    })?;
                if status.success() {
                    return Ok(());
                }
                bail!(
                    "{} failed extracting {} (rust extract also failed: {rust_err:#})",
                    tar_bin.display(),
                    archive.display()
                );
            }
            return Err(rust_err).context(
                "extract release archive (no tar binary on PATH either; install tar or use a full agent build)",
            );
        }
    }
}

fn extract_tarball_rust(archive: &Path, dest: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let file = fs::File::open(archive)
        .with_context(|| format!("open release archive {}", archive.display()))?;
    let mut archive = Archive::new(GzDecoder::new(file));
    archive
        .unpack(dest)
        .with_context(|| format!("unpack release archive into {}", dest.display()))?;

    // Some releases nest files under a top-level directory — flatten one level if needed.
    let direct_bin = dest.join("scalattice-agent");
    if direct_bin.is_file() {
        return Ok(());
    }
    let entries: Vec<_> = fs::read_dir(dest)
        .with_context(|| format!("read staging {}", dest.display()))?
        .filter_map(|e| e.ok())
        .collect();
    if entries.len() == 1 && entries[0].path().is_dir() {
        let nested = entries[0].path();
        let nested_bin = nested.join("scalattice-agent");
        if nested_bin.is_file() {
            for entry in fs::read_dir(&nested).context("read nested release directory")? {
                let entry = entry?;
                let name = entry.file_name();
                let from = entry.path();
                let to = dest.join(&name);
                if from.is_dir() {
                    copy_dir_recursive(&from, &to)?;
                } else {
                    fs::copy(&from, &to)
                        .with_context(|| format!("flatten {}", name.to_string_lossy()))?;
                }
            }
        }
    }
    Ok(())
}

fn copy_dir_recursive(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

fn resolve_tar_binary() -> Option<PathBuf> {
    const CANDIDATES: &[&str] = &["/usr/bin/tar", "/bin/tar", "/usr/local/bin/tar"];
    for path in CANDIDATES {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    // Last resort: whatever PATH the process has (often empty/minimal under systemd).
    Command::new("tar")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| PathBuf::from("tar"))
}

fn apply_update(staging: &Path) -> Result<()> {
    let source_bin = staging.join("scalattice-agent");
    if !source_bin.is_file() {
        bail!(
            "release archive did not contain scalattice-agent at {}",
            source_bin.display()
        );
    }

    // Remote/website update runs inside `foreground` (the live agent). Stopping the
    // systemd unit here kills this process before the binary is replaced — that is
    // why Linux force-update from the dashboard failed while Windows (detached
    // installer) worked. CLI `scalattice-agent update` is a separate process and
    // should still stop the service first.
    let self_replace = running_as_live_agent();
    if self_replace {
        eprintln!(
            "self-update: replacing binary in place (not stopping this process)"
        );
    } else {
        service::stop_background_for_update()?;
    }

    // Linux: rename over ~/.local/bin is enough (systemd unit points there).
    // macOS: launchd often execs the .app bundle binary, not ~/.local/bin —
    // replacing only install_dir left KeepAlive restarting the old image.
    let targets = unix_agent_install_targets().context("resolve install targets")?;
    let mut replaced = 0usize;
    let mut last_err: Option<anyhow::Error> = None;
    for dest_bin in &targets {
        match replace_unix_binary(&source_bin, dest_bin) {
            Ok(()) => replaced += 1,
            Err(err) => {
                eprintln!(
                    "self-update: could not replace {}: {err:#}",
                    dest_bin.display()
                );
                last_err = Some(err);
            }
        }
    }
    if replaced == 0 {
        if let Some(err) = last_err {
            return Err(err);
        }
        bail!("failed to replace scalattice-agent at any install location");
    }

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

    if self_replace {
        // Caller (remote control) acks then restarts/exits so systemd picks up the
        // new binary. Restarting here would race the websocket ack.
    } else {
        service::restart_background_after_update()?;
    }
    fs::remove_dir_all(staging.parent().unwrap_or(staging)).ok();
    Ok(())
}

/// True when this process is the long-lived agent (`foreground`), not the CLI updater.
fn running_as_live_agent() -> bool {
    std::env::args().any(|arg| arg == "foreground")
}

fn replace_unix_binary(source: &Path, dest: &Path) -> Result<()> {
    let parent = dest.parent().context("agent binary parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create install directory {}", parent.display()))?;
    let tmp_bin = parent.join(format!(
        "{}.update.{}",
        dest.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("scalattice-agent"),
        std::process::id()
    ));
    // Atomic replace via rename is safe while this process is still mapped to the
    // old inode; Linux keeps the running image until exit. macOS usually allows
    // rename-over of a running Mach-O; if it does not, unlink then copy.
    fs::copy(source, &tmp_bin)
        .with_context(|| format!("copy new agent binary to {}", tmp_bin.display()))?;
    fs::set_permissions(&tmp_bin, fs::Permissions::from_mode(0o755))
        .context("set executable bit on new agent binary")?;
    match fs::rename(&tmp_bin, dest) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            let _ = fs::remove_file(dest);
            let copied = fs::copy(&tmp_bin, dest).and_then(|_| {
                fs::set_permissions(dest, fs::Permissions::from_mode(0o755))
            });
            let _ = fs::remove_file(&tmp_bin);
            copied.with_context(|| {
                format!(
                    "replace {} (rename failed: {rename_err})",
                    dest.display()
                )
            })?;
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn write_update_units() -> Result<()> {
    let home = crate::paths::os_user_home()?;
    let unit_dir = home.join(".config/systemd/user");
    fs::create_dir_all(&unit_dir).context("create systemd user unit directory")?;

    let bin = crate::paths::resolve_agent_binary().unwrap_or_else(|_| {
        crate::paths::install_dir()
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
RandomizedDelaySec=12h\n\
\n\
[Install]\n\
WantedBy=timers.target\n";

    fs::write(unit_dir.join(UPDATE_SERVICE), service)?;
    fs::write(unit_dir.join(UPDATE_TIMER), timer)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_update_units() -> Result<()> {
    let home = crate::paths::os_user_home()?;
    let unit_dir = home.join(".config/systemd/user");
    let _ = fs::remove_file(unit_dir.join(UPDATE_SERVICE));
    let _ = fs::remove_file(unit_dir.join(UPDATE_TIMER));
    Ok(())
}

#[cfg(target_os = "linux")]
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
