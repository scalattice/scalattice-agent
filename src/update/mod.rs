mod cloud;
mod version;

#[cfg(any(target_os = "linux", target_os = "macos"))]
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
        format!(
            "Update available: v{} (you have v{})",
            info.latest_version, info.current_version
        )
    } else {
        format!("Up to date (v{})", info.current_version)
    }
}
