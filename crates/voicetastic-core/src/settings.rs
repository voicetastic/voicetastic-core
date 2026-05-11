//! Persistent app settings (last-used device address, etc.).
//!
//! Stored as TOML under `$XDG_CONFIG_HOME/voicetastic/config.toml`,
//! falling back to `$HOME/.config/voicetastic/config.toml`. All fields are
//! optional so a malformed or missing file just yields defaults.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default cap for voice-message recording duration.
pub const DEFAULT_VOICE_MAX_SECS: u32 = 30;
/// Hard upper bound the UI exposes for the voice-message duration cap.
/// Anything larger would reliably exceed the protocol's per-message size
/// budget at typical voice bitrates.
pub const VOICE_MAX_SECS_UPPER: u32 = 120;

/// Default per-message reassembly timeout, in seconds. Matches the core
/// `AssemblerConfig` default and is exposed in the GUI so users can extend
/// it on slow modem presets where a single voice message may take minutes.
pub const DEFAULT_REASSEMBLY_TIMEOUT_SECS: u32 = 600;
/// Lower bound for the configurable reassembly timeout (10 s).
pub const REASSEMBLY_TIMEOUT_LOWER_SECS: u32 = 10;
/// Upper bound for the configurable reassembly timeout (1 hour).
pub const REASSEMBLY_TIMEOUT_UPPER_SECS: u32 = 3_600;

/// Persistent app preferences.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppSettings {
    /// Last successfully connected BLE address or serial port path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_device: Option<String>,

    /// Maximum recording duration (seconds) for voice messages composed in
    /// the GUI. `None` falls back to [`DEFAULT_VOICE_MAX_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_voice_duration_secs: Option<u32>,

    /// Per-message voice reassembly timeout (seconds). `None` falls back to
    /// [`DEFAULT_REASSEMBLY_TIMEOUT_SECS`]. Clamped to
    /// `[REASSEMBLY_TIMEOUT_LOWER_SECS, REASSEMBLY_TIMEOUT_UPPER_SECS]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reassembly_timeout_secs: Option<u32>,
}

impl AppSettings {
    /// Effective voice-message duration cap, clamped to a sane range.
    pub fn voice_max_secs(&self) -> u32 {
        self.max_voice_duration_secs
            .unwrap_or(DEFAULT_VOICE_MAX_SECS)
            .clamp(1, VOICE_MAX_SECS_UPPER)
    }

    /// Effective per-message reassembly timeout, clamped to a sane range.
    pub fn reassembly_timeout_secs(&self) -> u32 {
        self.reassembly_timeout_secs
            .unwrap_or(DEFAULT_REASSEMBLY_TIMEOUT_SECS)
            .clamp(REASSEMBLY_TIMEOUT_LOWER_SECS, REASSEMBLY_TIMEOUT_UPPER_SECS)
    }

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
