mod cloud;
mod space;
mod version;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod linux;
#[cfg(windows)]
mod windows;

pub use space::{ensure_disk_for_update, free_bytes_needed_for_asset};
pub use version::{compare_versions, current_version, normalize_version};

use std::time::{Duration, Instant};
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub latest_tag: String,
    pub update_available: bool,
}

#[derive(Debug, Clone)]
pub enum UpdateCheckOutcome {
    UpToDate(UpdateInfo),
    UpdateAvailable(UpdateInfo),
}

impl UpdateCheckOutcome {
    pub fn info(&self) -> &UpdateInfo {
        match self {
            Self::UpToDate(info) | Self::UpdateAvailable(info) => info,
        }
    }
}

pub async fn check_for_update() -> anyhow::Result<UpdateCheckOutcome> {
    #[cfg(windows)]
    {
        return windows::check_for_update().await;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        return linux::check_for_update().await;
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        anyhow::bail!("automatic updates are not supported on this platform");
    }
}

pub async fn install_latest_update() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        return windows::install_latest_update().await;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        return linux::install_latest_update().await;
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        anyhow::bail!("automatic updates are not supported on this platform");
    }
}

/// Apply the persisted auto-update setting to the platform (tray on Windows, systemd timer on Linux).
pub fn sync_auto_update(enabled: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        let _ = enabled;
        Ok(())
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        linux::sync_auto_update_timer(enabled)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = enabled;
        Ok(())
    }
}

pub fn maybe_sync_auto_update_timer() -> anyhow::Result<()> {
    let settings = crate::settings::UserSettings::load();
    sync_auto_update(settings.auto_update)
}

pub fn format_update_status(outcome: &UpdateCheckOutcome) -> String {
    let info = outcome.info();
    if info.update_available {
        match compare_versions(&info.latest_version, &info.current_version) {
            std::cmp::Ordering::Less => format!(
                "Rollback available: v{} (you have v{})",
                info.latest_version, info.current_version
            ),
            _ => format!(
                "Update available: v{} (you have v{})",
                info.latest_version, info.current_version
            ),
        }
    } else {
        format!("Up to date (v{})", info.current_version)
    }
}

/// Resolve how much free disk an update needs for `asset_name`.
pub(crate) async fn disk_need_for_asset(
    latest: &cloud::LatestRelease,
    asset_name: &str,
) -> u64 {
    let mut compressed = latest.sizes.get(asset_name).copied().unwrap_or(0);
    if compressed == 0 {
        if let Some(probed) = cloud::probe_release_asset_size(&latest.tag, asset_name).await {
            info!(
                asset = asset_name,
                bytes = probed,
                "probed release asset size via HEAD"
            );
            compressed = probed;
        }
    }
    let need = free_bytes_needed_for_asset(compressed);
    info!(
        asset = asset_name,
        compressed_mb = compressed / (1024 * 1024),
        need_mb = need / (1024 * 1024),
        "update free-space requirement"
    );
    need
}

/// After the new binary is downloaded, wait until in-flight jobs finish before
/// replacing the running agent. Refuses new claims while draining.
///
/// Does not cancel work: better to delay the update than abandon a paid job.
pub(crate) async fn wait_until_safe_to_apply() -> anyhow::Result<()> {
    crate::state::begin_update_drain();
    println!("Waiting for in-flight jobs to finish before applying update…");
    info!("update drain: waiting for in-flight jobs before apply/restart");

    let started = Instant::now();
    let mut last_log = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);
    // Cap wait so a wedged job cannot block updates forever; leave the download
    // staged and fail so a later retry can apply once the machine is idle.
    let max_wait = Duration::from_secs(2 * 60 * 60);
    let live = std::env::args().any(|arg| arg == "foreground");

    loop {
        let jobs = observed_active_jobs();
        let agent_ack = live || agent_has_acked_drain();
        if agent_ack && jobs == 0 && !status_looks_busy() {
            info!(
                waited_secs = started.elapsed().as_secs(),
                "update drain: idle; applying"
            );
            println!("No in-flight jobs — applying update.");
            return Ok(());
        }
        if started.elapsed() >= max_wait {
            crate::state::end_update_drain();
            anyhow::bail!(
                "timed out after {}m waiting for {jobs} in-flight job(s) to finish; \
                 update download is ready and will apply on the next idle attempt",
                max_wait.as_secs() / 60
            );
        }
        if last_log.elapsed() >= Duration::from_secs(30) {
            last_log = Instant::now();
            warn!(
                jobs,
                agent_ack,
                waited_secs = started.elapsed().as_secs(),
                "update drain: still waiting for jobs"
            );
            if !agent_ack {
                println!(
                    "Waiting for the running agent to acknowledge update drain ({:.0}s)…",
                    started.elapsed().as_secs_f64()
                );
            } else {
                println!(
                    "Still waiting for {jobs} in-flight job(s) before update ({:.0}s)…",
                    started.elapsed().as_secs_f64()
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn observed_active_jobs() -> u32 {
    // Same process (remote update from foreground): use the live counter.
    let local = crate::state::reported_active_jobs();
    if local > 0 {
        return local;
    }
    // CLI / tray / systemd timer: read what the background agent last wrote.
    crate::state::read_state()
        .map(|s| s.active_job_count)
        .unwrap_or(0)
        .max(local)
}

fn agent_has_acked_drain() -> bool {
    if crate::state::read_state()
        .map(|s| s.update_draining)
        .unwrap_or(false)
    {
        return true;
    }
    // Nothing running to abandon — CLI/tray can apply immediately.
    !crate::service::service_active()
}

fn status_looks_busy() -> bool {
    let Some(state) = crate::state::read_state() else {
        return false;
    };
    if state.active_job_count > 0 {
        return true;
    }
    let label = state.status_label.unwrap_or_default();
    let lower = label.to_ascii_lowercase();
    lower.contains("running") || lower.contains("busy") || lower.contains("inferenc")
}
