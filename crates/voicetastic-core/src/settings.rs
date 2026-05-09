//! Persistent app settings (last-used device address, etc.).
//!
//! Stored as TOML under `$XDG_CONFIG_HOME/voicetastic/config.toml`,
//! falling back to `$HOME/.config/voicetastic/config.toml`. All fields are
//! optional so a malformed or missing file just yields defaults.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Persistent app preferences.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppSettings {
    /// Last successfully connected BLE address or serial port path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_device: Option<String>,
}

impl AppSettings {
    /// Load from the config path, or return defaults if the file is missing
    /// or unparseable. Errors are intentionally swallowed — corrupt config
    /// must never block app startup.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let Ok(s) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str(&s).unwrap_or_default()
    }

    /// Persist to the config path. Returns `Err` if the directory can't be
    /// created or the write fails; callers may choose to surface or ignore.
    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no config dir available")
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&path, body)
    }
}

/// Resolve `$XDG_CONFIG_HOME/voicetastic/config.toml`, falling back to
/// `$HOME/.config/voicetastic/config.toml`. Returns `None` if neither env
/// var is set (e.g. on a headless container with no `$HOME`).
fn config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("voicetastic/config.toml"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/voicetastic/config.toml"))
}
