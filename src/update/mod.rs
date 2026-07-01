mod github;
mod version;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

pub use version::{compare_versions, current_version, normalize_version};

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

const GITHUB_REPO: &str = "Robottik-Software/scalattice-agent";

pub async fn check_for_update() -> anyhow::Result<UpdateCheckOutcome> {
    #[cfg(windows)]
    {
        return windows::check_for_update().await;
    }
    #[cfg(target_os = "linux")]
    {
        return linux::check_for_update().await;
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = GITHUB_REPO;
        anyhow::bail!("automatic updates are not supported on this platform");
    }
}

pub async fn install_latest_update() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        return windows::install_latest_update().await;
    }
    #[cfg(target_os = "linux")]
    {
        return linux::install_latest_update().await;
    }
    #[cfg(not(any(windows, target_os = "linux")))]
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
    #[cfg(target_os = "linux")]
    {
        linux::sync_auto_update_timer(enabled)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
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
        format!(
            "Update available: v{} (you have v{})",
            info.latest_version, info.current_version
        )
    } else {
        format!("Up to date (v{})", info.current_version)
    }
}
