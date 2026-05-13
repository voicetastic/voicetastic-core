//! Centralised settings API consumed by the GUI, CLI and Android bridge.
//!
//! Front-ends never poke [`AppSettings`] fields directly: they hold an
//! [`Arc<SettingsApi>`] and call typed getters / setters. The API takes
//! care of validation, persistence, and notifying listeners (e.g. the
//! voice runtime) when a value that affects live state changes.
//!
//! Persistence path resolution is host-injected: desktop callers use
//! [`SettingsApi::open`] (which picks `$XDG_CONFIG_HOME/voicetastic/
//! config.toml`); Android passes its per-app data directory via
//! [`SettingsApi::open_at`]; tests may use [`SettingsApi::in_memory`].

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::voice::VoiceCodec;

use super::data::{
    AMRNB_MODE_1220, AppSettings, CODEC2_MODE_1200, DEFAULT_AMRNB_MODE, DEFAULT_CODEC2_MODE,
    DEFAULT_OPUS_BANDWIDTH, DEFAULT_OPUS_BITRATE_KBPS, DEFAULT_REASSEMBLY_TIMEOUT_SECS,
    DEFAULT_VOICE_CODEC, DEFAULT_VOICE_MAX_SECS, OPUS_BANDWIDTH_NARROW, OPUS_BANDWIDTH_WIDE,
    OPUS_BITRATE_KBPS_MAX, OPUS_BITRATE_KBPS_MIN, REASSEMBLY_TIMEOUT_LOWER_SECS,
    REASSEMBLY_TIMEOUT_UPPER_SECS, VOICE_CODEC_AMRNB, VOICE_CODEC_CODEC2, VOICE_CODEC_OPUS,
    VOICE_MAX_SECS_UPPER, config_path,
};

// ---------------------------------------------------------------------------
// Field key
// ---------------------------------------------------------------------------

/// Stable identifier for every persisted client setting. Used as the
/// generic-access key for the CLI and as the listener event payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingKey {
    /// Last successfully connected BLE address / serial path.
    LastDevice,
    /// Voice-message recording duration cap (seconds).
    VoiceMaxDurationSecs,
    /// Voice reassembly timeout (seconds).
    VoiceReassemblyTimeoutSecs,
    /// Outgoing voice codec id.
    VoiceCodec,
    /// Codec2 mode index (0..=5).
    VoiceCodec2Mode,
    /// AMR-NB mode index (0..=7).
    VoiceAmrnbMode,
    /// Opus encoder bitrate in kbps.
    VoiceOpusBitrateKbps,
    /// Opus audio bandwidth (`narrow` | `wide`).
    VoiceOpusBandwidth,
}

impl SettingKey {
    /// Wire-stable string id (used by the CLI and the bridge).
    pub fn id(self) -> &'static str {
        match self {
            Self::LastDevice => "last_device",
            Self::VoiceMaxDurationSecs => "voice.max_duration_secs",
            Self::VoiceReassemblyTimeoutSecs => "voice.reassembly_timeout_secs",
            Self::VoiceCodec => "voice.codec",
            Self::VoiceCodec2Mode => "voice.codec2_mode",
            Self::VoiceAmrnbMode => "voice.amrnb_mode",
            Self::VoiceOpusBitrateKbps => "voice.opus_bitrate_kbps",
            Self::VoiceOpusBandwidth => "voice.opus_bandwidth",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Some(match s {
            "last_device" => Self::LastDevice,
            "voice.max_duration_secs" => Self::VoiceMaxDurationSecs,
            "voice.reassembly_timeout_secs" => Self::VoiceReassemblyTimeoutSecs,
            "voice.codec" => Self::VoiceCodec,
            "voice.codec2_mode" => Self::VoiceCodec2Mode,
            "voice.amrnb_mode" => Self::VoiceAmrnbMode,
            "voice.opus_bitrate_kbps" => Self::VoiceOpusBitrateKbps,
            "voice.opus_bandwidth" => Self::VoiceOpusBandwidth,
            _ => return None,
        })
    }

    pub fn all() -> &'static [SettingKey] {
        &[
            SettingKey::LastDevice,
            SettingKey::VoiceMaxDurationSecs,
            SettingKey::VoiceReassemblyTimeoutSecs,
            SettingKey::VoiceCodec,
            SettingKey::VoiceCodec2Mode,
            SettingKey::VoiceAmrnbMode,
            SettingKey::VoiceOpusBitrateKbps,
            SettingKey::VoiceOpusBandwidth,
        ]
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("unknown setting `{0}`")]
    UnknownKey(String),
    #[error("setting `{key}` rejected value `{value}`: {reason}")]
    Invalid {
        key: &'static str,
        value: String,
        reason: String,
    },
    #[error("failed to persist settings: {0}")]
    Io(#[from] std::io::Error),
}

pub type SettingsResult<T> = Result<T, SettingsError>;

// ---------------------------------------------------------------------------
// Descriptor (for generic UIs / CLI listing)
// ---------------------------------------------------------------------------

/// Kind of value a setting accepts. Used by `list()` so a generic front-
/// end (CLI `settings list`) can render and validate without hard-coding
/// each field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingKind {
    /// Free-form optional UTF-8 string. Empty input clears the value.
    OptionalString,
    /// Integer in `[min, max]`.
    IntRange { min: u32, max: u32 },
    /// Enumerated, e.g. codec id. `variants` lists every accepted token.
    Enum { variants: Vec<&'static str> },
}

/// Static + dynamic metadata about one setting, suitable for rendering
/// in a generic list. `value` and `default` are stringified using the
/// same format `set_str` accepts.
#[derive(Debug, Clone)]
pub struct SettingDescriptor {
    pub key: SettingKey,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: SettingKind,
    pub value: String,
    pub default: String,
}

// ---------------------------------------------------------------------------
// Listener
// ---------------------------------------------------------------------------

/// Notified after a setting value changes. Listeners are called with the
/// write lock released, so it's safe to call back into the API.
pub trait SettingsListener: Send + Sync {
    fn on_change(&self, key: SettingKey);
}

// ---------------------------------------------------------------------------
// Voice codec enum (typed mirror of the wire/id string)
// ---------------------------------------------------------------------------

/// Typed mirror of the `voice.codec` string id. Front-ends are expected
/// to use this rather than raw `&str`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceCodecKind {
    Opus,
    Codec2,
    AmrNb,
}

impl VoiceCodecKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Opus => VOICE_CODEC_OPUS,
            Self::Codec2 => VOICE_CODEC_CODEC2,
            Self::AmrNb => VOICE_CODEC_AMRNB,
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Some(match s {
            VOICE_CODEC_OPUS => Self::Opus,
            VOICE_CODEC_CODEC2 => Self::Codec2,
            VOICE_CODEC_AMRNB => Self::AmrNb,
            _ => return None,
        })
    }
}

/// Free-function aliases retained for the bridge / FFI which prefers
/// plain functions over methods on enums.
pub fn voice_codec_kind_to_id(k: VoiceCodecKind) -> &'static str {
    k.id()
}

pub fn voice_codec_kind_from_id(s: &str) -> Option<VoiceCodecKind> {
    VoiceCodecKind::from_id(s)
}

/// Typed mirror of the `voice.opus_bandwidth` string id. Only the two
/// modes useful for LoRa-voice are exposed; super-wide and full-band
/// are deliberately omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusBandwidthKind {
    /// SILK 8 kHz operating mode (telephony quality, lowest airtime).
    Narrow,
    /// SILK 16 kHz operating mode (HD voice, default).
    Wide,
}

impl OpusBandwidthKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Narrow => OPUS_BANDWIDTH_NARROW,
            Self::Wide => OPUS_BANDWIDTH_WIDE,
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Some(match s {
            OPUS_BANDWIDTH_NARROW => Self::Narrow,
            OPUS_BANDWIDTH_WIDE => Self::Wide,
            _ => return None,
        })
    }
}

pub fn opus_bandwidth_kind_to_id(k: OpusBandwidthKind) -> &'static str {
    k.id()
}

pub fn opus_bandwidth_kind_from_id(s: &str) -> Option<OpusBandwidthKind> {
    OpusBandwidthKind::from_id(s)
}

// ---------------------------------------------------------------------------
// Settings API
// ---------------------------------------------------------------------------

/// Central settings facade. Cheap to clone (it's an `Arc` internally
/// via [`SettingsApi::open`]); listeners and persistence are shared.
pub struct SettingsApi {
    inner: RwLock<AppSettings>,
    /// Persistence path. `None` means in-memory only (tests, headless).
    path: RwLock<Option<PathBuf>>,
    listeners: Mutex<Vec<Arc<dyn SettingsListener>>>,
}

impl SettingsApi {
    /// Open the desktop default store at `$XDG_CONFIG_HOME/voicetastic/
    /// config.toml` (falling back to `$HOME/.config/...`). When neither
    /// is set the API runs in-memory.
    pub fn open() -> Arc<Self> {
        Self::open_at(config_path())
    }

    /// Open at an explicit path. Pass `None` to run in-memory.
    pub fn open_at(path: Option<PathBuf>) -> Arc<Self> {
        let data = match path.as_ref() {
            Some(p) => std::fs::read_to_string(p)
                .ok()
                .and_then(|s| toml::from_str(&s).ok())
                .unwrap_or_default(),
            None => AppSettings::default(),
        };
        Arc::new(Self {
            inner: RwLock::new(data),
            path: RwLock::new(path),
            listeners: Mutex::new(Vec::new()),
        })
    }

    /// In-memory store. Useful for tests and the Android bridge while it
    /// is bootstrapping (set the path later with [`set_path`]).
    pub fn in_memory() -> Arc<Self> {
        Self::open_at(None)
    }

    /// Override the persistence path. Does **not** re-read; call
    /// [`reload`] explicitly if needed.
    pub fn set_path(&self, path: Option<PathBuf>) {
        *self.path.write() = path;
    }

    /// Reload from disk, discarding any in-memory edits.
    pub fn reload(&self) {
        let p = self.path.read().clone();
        let data = match p {
            Some(p) => std::fs::read_to_string(p)
                .ok()
                .and_then(|s| toml::from_str(&s).ok())
                .unwrap_or_default(),
            None => AppSettings::default(),
        };
        *self.inner.write() = data;
        for k in SettingKey::all() {
            self.notify(*k);
        }
    }

    /// Subscribe to value changes.
    pub fn subscribe(&self, listener: Arc<dyn SettingsListener>) {
        self.listeners.lock().push(listener);
    }

    /// Raw snapshot of the persisted struct. Read-only.
    pub fn snapshot(&self) -> AppSettings {
        self.inner.read().clone()
    }

    // -----------------------------------------------------------------
    // Typed getters (return effective, validated values)
    // -----------------------------------------------------------------

    pub fn last_device(&self) -> Option<String> {
        self.inner.read().last_device.clone()
    }

    pub fn voice_max_secs(&self) -> u32 {
        self.inner.read().voice_max_secs()
    }

    pub fn reassembly_timeout_secs(&self) -> u32 {
        self.inner.read().reassembly_timeout_secs()
    }

    pub fn voice_codec(&self) -> VoiceCodecKind {
        VoiceCodecKind::from_id(self.inner.read().voice_codec()).unwrap_or(VoiceCodecKind::AmrNb)
    }

    pub fn voice_codec2_mode(&self) -> u8 {
        self.inner.read().voice_codec2_mode()
    }

    pub fn voice_amrnb_mode(&self) -> u8 {
        self.inner.read().voice_amrnb_mode()
    }

    pub fn voice_opus_bitrate_kbps(&self) -> u8 {
        self.inner.read().voice_opus_bitrate_kbps()
    }

    pub fn voice_opus_bandwidth(&self) -> OpusBandwidthKind {
        OpusBandwidthKind::from_id(self.inner.read().voice_opus_bandwidth())
            .unwrap_or(OpusBandwidthKind::Wide)
    }

    /// Convenience: resolve `voice.codec` + per-codec mode to the
    /// `(VoiceCodec, codec_param)` pair the voice protocol layer wants.
    pub fn voice_codec_for_protocol(&self) -> (VoiceCodec, u8) {
        match self.voice_codec() {
            VoiceCodecKind::Opus => (VoiceCodec::Opus, self.voice_opus_bitrate_kbps()),
            VoiceCodecKind::Codec2 => (VoiceCodec::Codec2, self.voice_codec2_mode()),
            VoiceCodecKind::AmrNb => (VoiceCodec::AmrNb, self.voice_amrnb_mode()),
        }
    }

    // -----------------------------------------------------------------
    // Typed setters
    // -----------------------------------------------------------------

    pub fn set_last_device(&self, value: Option<String>) -> SettingsResult<()> {
        {
            let mut g = self.inner.write();
            g.last_device = value.filter(|s| !s.is_empty());
        }
        self.persist_and_notify(SettingKey::LastDevice)
    }

    pub fn set_voice_max_secs(&self, secs: u32) -> SettingsResult<()> {
        if !(1..=VOICE_MAX_SECS_UPPER).contains(&secs) {
            return Err(SettingsError::Invalid {
                key: SettingKey::VoiceMaxDurationSecs.id(),
                value: secs.to_string(),
                reason: format!("must be in 1..={VOICE_MAX_SECS_UPPER}"),
            });
        }
        self.inner.write().max_voice_duration_secs = Some(secs);
        self.persist_and_notify(SettingKey::VoiceMaxDurationSecs)
    }

    pub fn set_reassembly_timeout_secs(&self, secs: u32) -> SettingsResult<()> {
        if !(REASSEMBLY_TIMEOUT_LOWER_SECS..=REASSEMBLY_TIMEOUT_UPPER_SECS).contains(&secs) {
            return Err(SettingsError::Invalid {
                key: SettingKey::VoiceReassemblyTimeoutSecs.id(),
                value: secs.to_string(),
                reason: format!(
                    "must be in {REASSEMBLY_TIMEOUT_LOWER_SECS}..={REASSEMBLY_TIMEOUT_UPPER_SECS}"
                ),
            });
        }
        self.inner.write().reassembly_timeout_secs = Some(secs);
        self.persist_and_notify(SettingKey::VoiceReassemblyTimeoutSecs)
    }

    pub fn set_voice_codec(&self, kind: VoiceCodecKind) -> SettingsResult<()> {
        self.inner.write().voice_codec = Some(kind.id().to_string());
        self.persist_and_notify(SettingKey::VoiceCodec)
    }

    pub fn set_voice_codec2_mode(&self, mode: u8) -> SettingsResult<()> {
        if mode > CODEC2_MODE_1200 {
            return Err(SettingsError::Invalid {
                key: SettingKey::VoiceCodec2Mode.id(),
                value: mode.to_string(),
                reason: format!("must be in 0..={CODEC2_MODE_1200}"),
            });
        }
        self.inner.write().voice_codec2_mode = Some(mode);
        self.persist_and_notify(SettingKey::VoiceCodec2Mode)
    }

    pub fn set_voice_amrnb_mode(&self, mode: u8) -> SettingsResult<()> {
        if mode > AMRNB_MODE_1220 {
            return Err(SettingsError::Invalid {
                key: SettingKey::VoiceAmrnbMode.id(),
                value: mode.to_string(),
                reason: format!("must be in 0..={AMRNB_MODE_1220}"),
            });
        }
        self.inner.write().voice_amrnb_mode = Some(mode);
        self.persist_and_notify(SettingKey::VoiceAmrnbMode)
    }

    pub fn set_voice_opus_bitrate_kbps(&self, kbps: u8) -> SettingsResult<()> {
        if !(OPUS_BITRATE_KBPS_MIN..=OPUS_BITRATE_KBPS_MAX).contains(&kbps) {
            return Err(SettingsError::Invalid {
                key: SettingKey::VoiceOpusBitrateKbps.id(),
                value: kbps.to_string(),
                reason: format!("must be in {OPUS_BITRATE_KBPS_MIN}..={OPUS_BITRATE_KBPS_MAX}"),
            });
        }
        self.inner.write().voice_opus_bitrate_kbps = Some(kbps);
        self.persist_and_notify(SettingKey::VoiceOpusBitrateKbps)
    }

    pub fn set_voice_opus_bandwidth(&self, bw: OpusBandwidthKind) -> SettingsResult<()> {
        self.inner.write().voice_opus_bandwidth = Some(bw.id().to_string());
        self.persist_and_notify(SettingKey::VoiceOpusBandwidth)
    }

    /// Clear a single field's override (revert to its default).
    pub fn reset(&self, key: SettingKey) -> SettingsResult<()> {
        {
            let mut g = self.inner.write();
            match key {
                SettingKey::LastDevice => g.last_device = None,
                SettingKey::VoiceMaxDurationSecs => g.max_voice_duration_secs = None,
                SettingKey::VoiceReassemblyTimeoutSecs => g.reassembly_timeout_secs = None,
                SettingKey::VoiceCodec => g.voice_codec = None,
                SettingKey::VoiceCodec2Mode => g.voice_codec2_mode = None,
                SettingKey::VoiceAmrnbMode => g.voice_amrnb_mode = None,
                SettingKey::VoiceOpusBitrateKbps => g.voice_opus_bitrate_kbps = None,
                SettingKey::VoiceOpusBandwidth => g.voice_opus_bandwidth = None,
            }
        }
        self.persist_and_notify(key)
    }

    /// Reset every field at once.
    pub fn reset_all(&self) -> SettingsResult<()> {
        *self.inner.write() = AppSettings::default();
        self.persist()?;
        for k in SettingKey::all() {
            self.notify(*k);
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Generic (string) access for the CLI / generic UIs
    // -----------------------------------------------------------------

    /// Stringified current value for `key`. Same format `set_str`
    /// accepts. Returns the empty string for an unset `Option<_>` field.
    pub fn get_str(&self, key: SettingKey) -> String {
        match key {
            SettingKey::LastDevice => self.last_device().unwrap_or_default(),
            SettingKey::VoiceMaxDurationSecs => self.voice_max_secs().to_string(),
            SettingKey::VoiceReassemblyTimeoutSecs => self.reassembly_timeout_secs().to_string(),
            SettingKey::VoiceCodec => self.voice_codec().id().to_string(),
            SettingKey::VoiceCodec2Mode => self.voice_codec2_mode().to_string(),
            SettingKey::VoiceAmrnbMode => self.voice_amrnb_mode().to_string(),
            SettingKey::VoiceOpusBitrateKbps => self.voice_opus_bitrate_kbps().to_string(),
            SettingKey::VoiceOpusBandwidth => self.voice_opus_bandwidth().id().to_string(),
        }
    }

    /// Parse and apply a stringified value. Mirrors the CLI `settings
    /// set` UX and is also what the bridge uses for unknown-typed input.
    pub fn set_str(&self, key: SettingKey, value: &str) -> SettingsResult<()> {
        match key {
            SettingKey::LastDevice => self.set_last_device(if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }),
            SettingKey::VoiceMaxDurationSecs => {
                let n = parse_u32(key, value)?;
                self.set_voice_max_secs(n)
            }
            SettingKey::VoiceReassemblyTimeoutSecs => {
                let n = parse_u32(key, value)?;
                self.set_reassembly_timeout_secs(n)
            }
            SettingKey::VoiceCodec => {
                let kind =
                    VoiceCodecKind::from_id(value).ok_or_else(|| SettingsError::Invalid {
                        key: key.id(),
                        value: value.to_string(),
                        reason: format!(
                            "expected one of {VOICE_CODEC_AMRNB}, {VOICE_CODEC_CODEC2}, {VOICE_CODEC_OPUS}"
                        ),
                    })?;
                self.set_voice_codec(kind)
            }
            SettingKey::VoiceCodec2Mode => {
                let n = parse_u32(key, value)?;
                if n > u32::from(u8::MAX) {
                    return Err(SettingsError::Invalid {
                        key: key.id(),
                        value: value.to_string(),
                        reason: "value out of u8 range".to_string(),
                    });
                }
                self.set_voice_codec2_mode(n as u8)
            }
            SettingKey::VoiceAmrnbMode => {
                let n = parse_u32(key, value)?;
                if n > u32::from(u8::MAX) {
                    return Err(SettingsError::Invalid {
                        key: key.id(),
                        value: value.to_string(),
                        reason: "value out of u8 range".to_string(),
                    });
                }
                self.set_voice_amrnb_mode(n as u8)
            }
            SettingKey::VoiceOpusBitrateKbps => {
                let n = parse_u32(key, value)?;
                if n > u32::from(u8::MAX) {
                    return Err(SettingsError::Invalid {
                        key: key.id(),
                        value: value.to_string(),
                        reason: "value out of u8 range".to_string(),
                    });
                }
                self.set_voice_opus_bitrate_kbps(n as u8)
            }
            SettingKey::VoiceOpusBandwidth => {
                let bw =
                    OpusBandwidthKind::from_id(value).ok_or_else(|| SettingsError::Invalid {
                        key: key.id(),
                        value: value.to_string(),
                        reason: format!(
                            "expected one of {OPUS_BANDWIDTH_NARROW}, {OPUS_BANDWIDTH_WIDE}"
                        ),
                    })?;
                self.set_voice_opus_bandwidth(bw)
            }
        }
    }

    /// Descriptor list for every known setting, with current and default
    /// values pre-stringified. Order matches [`SettingKey::all`].
    pub fn list(&self) -> Vec<SettingDescriptor> {
        SettingKey::all()
            .iter()
            .map(|k| self.describe(*k))
            .collect()
    }

    fn describe(&self, key: SettingKey) -> SettingDescriptor {
        let (label, help, kind, default) = match key {
            SettingKey::LastDevice => (
                "Last device",
                "BLE address or serial port path remembered from the last successful connection.",
                SettingKind::OptionalString,
                String::new(),
            ),
            SettingKey::VoiceMaxDurationSecs => (
                "Max voice recording duration (s)",
                "Voice message capture stops automatically when this duration is reached.",
                SettingKind::IntRange {
                    min: 1,
                    max: VOICE_MAX_SECS_UPPER,
                },
                DEFAULT_VOICE_MAX_SECS.to_string(),
            ),
            SettingKey::VoiceReassemblyTimeoutSecs => (
                "Voice reassembly timeout (s)",
                "How long the receiver waits for missing chunks of an in-flight voice message.",
                SettingKind::IntRange {
                    min: REASSEMBLY_TIMEOUT_LOWER_SECS,
                    max: REASSEMBLY_TIMEOUT_UPPER_SECS,
                },
                DEFAULT_REASSEMBLY_TIMEOUT_SECS.to_string(),
            ),
            SettingKey::VoiceCodec => (
                "Outgoing voice codec",
                "Codec used when sending new voice messages. Received messages always decode with the codec advertised in their header.",
                SettingKind::Enum {
                    variants: vec![VOICE_CODEC_AMRNB, VOICE_CODEC_CODEC2, VOICE_CODEC_OPUS],
                },
                DEFAULT_VOICE_CODEC.to_string(),
            ),
            SettingKey::VoiceCodec2Mode => (
                "Codec2 bitrate mode",
                "Codec2 mode index (0=3200 bps .. 5=1200 bps). Lower is more LoRa-friendly.",
                SettingKind::IntRange {
                    min: 0,
                    max: u32::from(CODEC2_MODE_1200),
                },
                DEFAULT_CODEC2_MODE.to_string(),
            ),
            SettingKey::VoiceAmrnbMode => (
                "AMR-NB bitrate mode",
                "AMR-NB mode index (0=MR475 4.75 kbps .. 7=MR122 12.2 kbps). Lower is more LoRa-friendly.",
                SettingKind::IntRange {
                    min: 0,
                    max: u32::from(AMRNB_MODE_1220),
                },
                DEFAULT_AMRNB_MODE.to_string(),
            ),
            SettingKey::VoiceOpusBitrateKbps => (
                "Opus bitrate (kbps)",
                "Opus encoder bitrate in kbps. Lower is more LoRa-friendly; 12 kbps fits a 30 s clip on every preset.",
                SettingKind::IntRange {
                    min: u32::from(OPUS_BITRATE_KBPS_MIN),
                    max: u32::from(OPUS_BITRATE_KBPS_MAX),
                },
                DEFAULT_OPUS_BITRATE_KBPS.to_string(),
            ),
            SettingKey::VoiceOpusBandwidth => (
                "Opus audio bandwidth",
                "Forces the Opus operating mode. `narrow` = SILK 8 kHz (telephony), `wide` = SILK 16 kHz (HD voice). Sender-only — receiver auto-detects.",
                SettingKind::Enum {
                    variants: vec![OPUS_BANDWIDTH_NARROW, OPUS_BANDWIDTH_WIDE],
                },
                DEFAULT_OPUS_BANDWIDTH.to_string(),
            ),
        };
        SettingDescriptor {
            key,
            label,
            help,
            kind,
            value: self.get_str(key),
            default,
        }
    }

    // -----------------------------------------------------------------
    // Persistence / notification plumbing
    // -----------------------------------------------------------------

    fn persist_and_notify(&self, key: SettingKey) -> SettingsResult<()> {
        self.persist()?;
        self.notify(key);
        Ok(())
    }

    fn persist(&self) -> SettingsResult<()> {
        let path = self.path.read().clone();
        let Some(path) = path else {
            return Ok(()); // in-memory only
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(&*self.inner.read())
            .map_err(|e| SettingsError::Io(std::io::Error::other(e)))?;
        std::fs::write(&path, body)?;
        Ok(())
    }

    fn notify(&self, key: SettingKey) {
        let listeners: Vec<_> = self.listeners.lock().iter().cloned().collect();
        for l in listeners {
            l.on_change(key);
        }
    }
}

fn parse_u32(key: SettingKey, value: &str) -> SettingsResult<u32> {
    value.parse::<u32>().map_err(|e| SettingsError::Invalid {
        key: key.id(),
        value: value.to_string(),
        reason: e.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_round_trip() {
        let api = SettingsApi::in_memory();
        assert_eq!(api.voice_max_secs(), DEFAULT_VOICE_MAX_SECS);
        api.set_voice_max_secs(45).unwrap();
        assert_eq!(api.voice_max_secs(), 45);
        api.reset(SettingKey::VoiceMaxDurationSecs).unwrap();
        assert_eq!(api.voice_max_secs(), DEFAULT_VOICE_MAX_SECS);
    }

    #[test]
    fn rejects_out_of_range() {
        let api = SettingsApi::in_memory();
        let err = api
            .set_voice_max_secs(VOICE_MAX_SECS_UPPER + 1)
            .unwrap_err();
        assert!(matches!(err, SettingsError::Invalid { .. }));
    }

    #[test]
    fn set_str_round_trip() {
        let api = SettingsApi::in_memory();
        api.set_str(SettingKey::VoiceCodec, VOICE_CODEC_OPUS)
            .unwrap();
        assert_eq!(api.voice_codec(), VoiceCodecKind::Opus);
        assert_eq!(api.get_str(SettingKey::VoiceCodec), VOICE_CODEC_OPUS);
    }

    #[test]
    fn persists_to_disk() {
        let tmp = std::env::temp_dir().join(format!(
            "voicetastic-settings-test-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp);
        {
            let api = SettingsApi::open_at(Some(tmp.clone()));
            api.set_voice_codec(VoiceCodecKind::Opus).unwrap();
        }
        let api = SettingsApi::open_at(Some(tmp.clone()));
        assert_eq!(api.voice_codec(), VoiceCodecKind::Opus);
        let _ = std::fs::remove_file(&tmp);
    }

    struct Counter(std::sync::atomic::AtomicUsize);
    impl SettingsListener for Counter {
        fn on_change(&self, _key: SettingKey) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn listener_fires_on_change() {
        let api = SettingsApi::in_memory();
        let c = Arc::new(Counter(Default::default()));
        api.subscribe(c.clone());
        api.set_voice_max_secs(20).unwrap();
        api.set_voice_codec(VoiceCodecKind::Opus).unwrap();
        assert_eq!(c.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
