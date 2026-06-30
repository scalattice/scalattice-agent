use crate::paths::settings_path;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

pub const UPDATE_CHECK_INTERVAL_SECS: u64 = 86_400;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSettings {
    #[serde(default)]
    pub auto_update: bool,
    #[serde(default)]
    pub last_update_check_unix: u64,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            auto_update: false,
            last_update_check_unix: 0,
        }
    }
}

impl UserSettings {
    pub fn load() -> Self {
        let Ok(path) = settings_path() else {
            return Self::default();
        };
        if !path.is_file() {
            return Self::default();
        }
        fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
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

    pub fn should_check_for_update(&self) -> bool {
        let now = unix_now();
        now.saturating_sub(self.last_update_check_unix) >= UPDATE_CHECK_INTERVAL_SECS
    }

    pub fn mark_update_checked(&mut self) {
        self.last_update_check_unix = unix_now();
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
