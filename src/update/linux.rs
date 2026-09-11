use super::cloud::{download_release_asset, fetch_latest_release};
use super::{compare_versions, current_version, UpdateCheckOutcome, UpdateInfo};
use crate::service;
use anyhow::{bail, Context, Result};
use std::cmp::Ordering;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "linux")]
use crate::paths::{lib_dir, unix_agent_install_targets};

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

    // Never stop the live agent (or the CLI's background service) before the
    // download finishes. Doing that on macOS made the machine look frozen as
    // soon as a newer release was detected, and it also raced KeepAlive /
    // tray watchdog restarts while the archive was still downloading.
    #[cfg(target_os = "macos")]
    {
        // Replacing only the Mach-O inside a notarized .app invalidates the
        // bundle signature. Gatekeeper then shows "Unable to open the
        // application". Install the whole signed DMG instead (same artifact as
        // a manual download).
        println!(
            "Downloading Scalattice Agent v{} ({})...",
            info.latest_version,
            macos_dmg_name()
        );
        let dmg = download_macos_dmg(&info.latest_tag).await?;
        println!("Installing update from signed DMG...");
        apply_macos_dmg_update(&dmg)?;
    }
    #[cfg(target_os = "linux")]
    {
        println!(
            "Downloading Scalattice Agent v{} ({})...",
            info.latest_version,
            unix_archive_name()?
        );
        let staging = download_and_extract(&info.latest_tag).await?;
        println!("Installing update...");
        apply_update(&staging)?;
    }
    if running_as_live_agent() {
        println!(
            "Updated to v{}. Live agent will restart onto the new binary.",
            info.latest_version
        );
    } else if crate::service::in_tray_process() {
        println!(
            "Updated to v{}. Restarting Scalattice Agent onto the new binary…",
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

#[cfg(target_os = "linux")]
fn unix_archive_name() -> Result<String> {
    Ok(format!(
        "scalattice-agent-{}.tar.gz",
        unix_release_target()?
    ))
}

#[cfg(target_os = "linux")]
fn unix_release_target() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64-unknown-linux-gnu"),
        "aarch64" => Ok("aarch64-unknown-linux-gnu"),
        other => bail!("automatic updates are not supported on Linux arch: {other}"),
    }
}

#[cfg(target_os = "macos")]
fn macos_dmg_name() -> &'static str {
    "ScalatticeAgentSetup-aarch64.dmg"
}

#[cfg(target_os = "macos")]
fn macos_app_name() -> &'static str {
    "Scalattice Agent.app"
}

#[cfg(target_os = "macos")]
fn macos_app_install_root() -> PathBuf {
    // CI update smoke installs under an isolated HOME; production uses /Applications.
    if let Ok(root) = std::env::var("SCALATTICE_MACOS_APP_ROOT") {
        let trimmed = root.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    PathBuf::from("/Applications")
}

#[cfg(target_os = "macos")]
fn macos_applications_app() -> PathBuf {
    macos_app_install_root().join(macos_app_name())
}

#[cfg(target_os = "macos")]
async fn download_macos_dmg(tag: &str) -> Result<PathBuf> {
    let latest = fetch_latest_release().await?;
    let dmg_name = macos_dmg_name();
    let expected = latest.checksums.get(dmg_name).cloned().with_context(|| {
        format!("Cloud release {tag} has no SHA-256 checksum for {dmg_name}; refusing to update")
    })?;
    let work = update_work_dir(tag)?;
    fs::create_dir_all(&work).context("create update work directory")?;
    let dmg_path = work.join(dmg_name);
    download_release_asset(tag, dmg_name, &dmg_path, &expected).await?;
    Ok(dmg_path)
}

/// Mount the notarized DMG and replace the whole `/Applications` bundle.
///
/// Swapping only `Contents/MacOS/scalattice-agent` breaks the Developer ID /
/// notarization seal; Gatekeeper then refuses to open the app ("Unable to open
/// the application").
#[cfg(target_os = "macos")]
fn apply_macos_dmg_update(dmg: &Path) -> Result<()> {
    let self_replace = running_as_live_agent();
    let tray_update = crate::service::in_tray_process();
    if !self_replace && !tray_update {
        service::stop_background_for_update()?;
    }

    let mount = dmg
        .parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .join(format!("scalattice-dmg-{}", std::process::id()));
    let _ = fs::remove_dir_all(&mount);
    fs::create_dir_all(&mount).context("create DMG mount point")?;

    let attach = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-mountpoint"])
        .arg(&mount)
        .arg(dmg)
        .output()
        .context("run hdiutil attach")?;
    if !attach.status.success() {
        let stderr = String::from_utf8_lossy(&attach.stderr);
        let _ = fs::remove_dir_all(&mount);
        bail!("hdiutil attach failed: {}", stderr.trim());
    }

    let install_result = (|| -> Result<()> {
        let bundled = mount.join(macos_app_name());
        if !bundled.is_dir() {
            bail!(
                "DMG does not contain {} at {}",
                macos_app_name(),
                bundled.display()
            );
        }
        let dest = macos_applications_app();
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).context("create /Applications")?;
        }
        // Stage next to /Applications then swap so a half-copied tree is never left
        // as the live bundle name.
        let staged = dest.with_extension(format!("app.updating.{}", std::process::id()));
        let _ = fs::remove_dir_all(&staged);
        let status = Command::new("ditto")
            .arg(&bundled)
            .arg(&staged)
            .status()
            .context("run ditto to stage updated .app")?;
        if !status.success() {
            let _ = fs::remove_dir_all(&staged);
            bail!("ditto failed copying {}", bundled.display());
        }
        // Drop quarantine if the download path stamped one (Gatekeeper still
        // validates Developer ID + notarization on the staged bundle).
        let _ = Command::new("xattr")
            .args(["-dr", "com.apple.quarantine"])
            .arg(&staged)
            .status();

        let backup = dest.with_extension(format!("app.bak.{}", std::process::id()));
        let _ = fs::remove_dir_all(&backup);
        if dest.exists() {
            fs::rename(&dest, &backup).with_context(|| format!("move aside {}", dest.display()))?;
        }
        if let Err(err) = fs::rename(&staged, &dest) {
            if backup.exists() {
                let _ = fs::rename(&backup, &dest);
            }
            let _ = fs::remove_dir_all(&staged);
            return Err(err).with_context(|| format!("activate {}", dest.display()));
        }
        let _ = fs::remove_dir_all(&backup);

        // Keep ~/.local/bin in sync for CLI / LaunchAgent paths that still point there.
        let app_bin = dest.join("Contents/MacOS/scalattice-agent");
        if app_bin.is_file() {
            if let Ok(local) = crate::paths::install_dir().map(|d| d.join("scalattice-agent")) {
                if let Err(err) = replace_unix_binary(&app_bin, &local) {
                    eprintln!(
                        "self-update: could not refresh {}: {err:#}",
                        local.display()
                    );
                }
            }
        }
        Ok(())
    })();

    let _ = Command::new("hdiutil")
        .args(["detach", "-quiet"])
        .arg(&mount)
        .status();
    let _ = fs::remove_dir_all(&mount);
    // Best-effort: DMG file can go after install.
    if let Some(parent) = dmg.parent() {
        let _ = fs::remove_dir_all(parent);
    }

    install_result?;

    if self_replace {
        // Remote control acks then restarts/exits.
    } else if tray_update {
        let _ = service::restart_background_after_update();
        relaunch_macos_tray_after_update();
        std::process::exit(0);
    } else {
        service::restart_background_after_update()?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn extract_tarball_rust(archive: &Path, dest: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let file = fs::File::open(archive)
        .with_context(|| format!("open release archive {}", archive.display()))?;
    let mut archive = Archive::new(GzDecoder::new(file));
    archive
        .unpack(dest)
        .with_context(|| format!("unpack release archive into {}", dest.display()))?;

    // Some releases nest files under a top-level directory: flatten one level if needed.
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn apply_update(staging: &Path) -> Result<()> {
    let source_bin = staging.join("scalattice-agent");
    if !source_bin.is_file() {
        bail!(
            "release archive did not contain scalattice-agent at {}",
            source_bin.display()
        );
    }

    // Remote/website update runs inside `foreground` (the live agent). Stopping the
    // systemd/launchd unit here kills this process before the binary is replaced  - 
    // that is why Linux force-update from the dashboard failed while Windows
    // (detached installer) worked. CLI `scalattice-agent update` is a separate
    // process and should still stop the service first: but only once we are
    // ready to replace files (download already finished above).
    let self_replace = running_as_live_agent();
    let tray_update = crate::service::in_tray_process();
    if !self_replace && !tray_update {
        service::stop_background_for_update()?;
    }

    // Linux: rename over ~/.local/bin is enough (systemd unit points there).
    // macOS: launchd often execs the .app bundle binary, not ~/.local/bin  - 
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
            let dest = dest_lib.join(&name);
            replace_unix_file(&entry.path(), &dest)
                .with_context(|| format!("replace library {}", name.to_string_lossy()))?;
        }
    }

    if self_replace {
        // Caller (remote control) acks then restarts/exits so systemd picks up the
        // new binary. Restarting here would race the websocket ack.
    } else if tray_update {
        // Tray already mapped the old binary. Restart the worker onto the new
        // image, then exit this tray process (Windows does the same via the
        // detached Inno setup). Relaunch the panel so the menu-bar icon returns.
        let _ = service::restart_background_after_update();
        fs::remove_dir_all(staging.parent().unwrap_or(staging)).ok();
        relaunch_macos_tray_after_update();
        std::process::exit(0);
    } else {
        service::restart_background_after_update()?;
    }
    fs::remove_dir_all(staging.parent().unwrap_or(staging)).ok();
    Ok(())
}

/// Best-effort: reopen the macOS tray app a moment after this process exits.
#[cfg(target_os = "macos")]
fn relaunch_macos_tray_after_update() {
    use std::os::unix::process::CommandExt;
    let app = PathBuf::from("/Applications/Scalattice Agent.app");
    if !app.is_dir() {
        return;
    }
    let mut cmd = Command::new("/bin/sh");
    // `open <path.app>` relaunches the bundle; `-a` expects a display name.
    cmd.arg("-c").arg(format!(
        "sleep 1; open {}",
        shell_single_quote(&app.display().to_string())
    ));
    let _ = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn();
}

#[cfg(target_os = "macos")]
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(not(target_os = "macos"))]
fn relaunch_macos_tray_after_update() {}

/// True when this process is the long-lived agent (`foreground`), not the CLI updater.
fn running_as_live_agent() -> bool {
    std::env::args().any(|arg| arg == "foreground")
}

fn replace_unix_binary(source: &Path, dest: &Path) -> Result<()> {
    replace_unix_file_with_mode(source, dest, Some(0o755))
}

/// Replace `dest` without truncating an inode a running process still has mapped.
///
/// `fs::copy` onto an existing file truncates that inode. The CLI updater is the
/// same CUDA-linked binary as the agent, with `$ORIGIN/../lib/scalattice` already
/// mapped, so overwriting those `.so` files in place SIGSEGVs after "success"
/// (`returncode -11`). Rename keeps the old inode alive until this process exits.
#[cfg(target_os = "linux")]
fn replace_unix_file(source: &Path, dest: &Path) -> Result<()> {
    replace_unix_file_with_mode(source, dest, None)
}

fn replace_unix_file_with_mode(source: &Path, dest: &Path, mode: Option<u32>) -> Result<()> {
    let parent = dest.parent().context("install parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create install directory {}", parent.display()))?;
    let tmp = parent.join(format!(
        "{}.update.{}",
        dest.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("scalattice-file"),
        std::process::id()
    ));
    fs::copy(source, &tmp).with_context(|| format!("copy new file to {}", tmp.display()))?;
    if let Some(mode) = mode {
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))
            .context("set permissions on new file")?;
    }
    match fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            // Unlink first so a fallback never writes through a mapped inode.
            let _ = fs::remove_file(dest);
            let replaced = fs::rename(&tmp, dest).or_else(|_| {
                fs::copy(&tmp, dest).and_then(|_| {
                    if let Some(mode) = mode {
                        fs::set_permissions(dest, fs::Permissions::from_mode(mode))?;
                    }
                    let _ = fs::remove_file(&tmp);
                    Ok(())
                })
            });
            replaced.with_context(|| {
                format!("replace {} (rename failed: {rename_err})", dest.display())
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
