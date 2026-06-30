use crate::paths::settings_path;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

pub const UPDATE_CHECK_INTERVAL_SECS: u64 = 86_400;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSettings {
    #[serde(default = "default_auto_update")]
    pub auto_update: bool,
    #[serde(default)]
    pub last_update_check_unix: u64,
    /// Seconds after UTC midnight when the daily update check should run (unique per install).
    #[serde(default)]
    pub update_daily_jitter_secs: u32,
}

fn default_auto_update() -> bool {
    true
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            auto_update: true,
            last_update_check_unix: 0,
            update_daily_jitter_secs: 0,
        }
    }
}

impl UserSettings {
    pub fn load() -> Self {
        let Ok(path) = settings_path() else {
            return Self::default_with_jitter();
        };
        if !path.is_file() {
            return Self::default_with_jitter();
        }
        let mut settings = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        settings.ensure_jitter();
        settings
    }

    pub fn save(&self) -> Result<()> {
        let path = settings_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("create settings directory")?;
        }
        let body = serde_json::to_string_pretty(self).context("serialize settings")?;
        fs::write(&path, format!("{body}\n")).context("write settings.json")?;
        Ok(())
    }

    fn default_with_jitter() -> Self {
        let mut settings = Self::default();
        settings.ensure_jitter();
        settings
    }

    pub fn ensure_jitter(&mut self) {
        if self.update_daily_jitter_secs == 0 {
            self.update_daily_jitter_secs = generate_update_jitter();
        }
    }

    pub fn should_check_for_update(&self) -> bool {
        self.seconds_until_update_check() == 0
    }

    pub fn seconds_until_update_check(&self) -> u64 {
        let jitter = self.update_daily_jitter_secs.max(1) as u64;
        let now = unix_now();
        let day_start = (now / UPDATE_CHECK_INTERVAL_SECS) * UPDATE_CHECK_INTERVAL_SECS;
        let mut scheduled = day_start + jitter;

        while scheduled <= self.last_update_check_unix {
            scheduled = scheduled.saturating_add(UPDATE_CHECK_INTERVAL_SECS);
        }

        if now >= scheduled {
            0
        } else {
            scheduled - now
        }
    }

    pub fn mark_update_checked(&mut self) {
        self.last_update_check_unix = unix_now();
    }
}

fn generate_update_jitter() -> u32 {
    let seed = unix_now()
        ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ settings_path()
            .ok()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
            .len() as u64;
    let jitter = (seed % UPDATE_CHECK_INTERVAL_SECS).max(60) as u32;
    jitter
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
