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
    DEFAULT_THEME_CONTRAST, DEFAULT_THEME_MODE, DEFAULT_VOICE_CODEC, DEFAULT_VOICE_DENOISE_ENABLED,
    DEFAULT_VOICE_FEC_MODE, DEFAULT_VOICE_MAX_SECS, DEFAULT_VOICE_NACK_MODE,
    DEFAULT_VOICE_PARTIAL_PLAY_ON_TIMEOUT, OPUS_BANDWIDTH_NARROW, OPUS_BANDWIDTH_WIDE,
    OPUS_BITRATE_KBPS_MAX, OPUS_BITRATE_KBPS_MIN, REASSEMBLY_TIMEOUT_LOWER_SECS,
    REASSEMBLY_TIMEOUT_UPPER_SECS, THEME_CONTRAST_HIGH, THEME_CONTRAST_STANDARD, THEME_MODE_DARK,
    THEME_MODE_LIGHT, THEME_MODE_SYSTEM, VOICE_CODEC_AMRNB, VOICE_CODEC_CODEC2, VOICE_CODEC_OPUS,
    VOICE_FEC_MODE_AUTO, VOICE_FEC_MODE_HEAVY, VOICE_FEC_MODE_LIGHT, VOICE_FEC_MODE_MEDIUM,
    VOICE_FEC_MODE_OFF, VOICE_MAX_SECS_UPPER, VOICE_NACK_MODE_AGGRESSIVE, VOICE_NACK_MODE_AUTO,
    VOICE_NACK_MODE_CONSERVATIVE, VOICE_NACK_MODE_OFF, config_path,
};

// ---------------------------------------------------------------------------
// Codec parameter type
// ---------------------------------------------------------------------------

/// Voice codec paired with its numeric parameter. Ensures codec and param are
/// always consistent (one lock acquisition, no mismatch risk).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceCodecParam {
    pub codec: VoiceCodec,
    pub param: u8,
}

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
    /// Capture-side RNNoise noise-suppression toggle.
    VoiceDenoiseEnabled,
    /// Receive-side: play partially-received voice when the reassembly
    /// timer fires for a never-completed message.
    VoicePartialPlayOnTimeout,
    /// Sender-side FEC parity policy.
    VoiceFecMode,
    /// Receive-side NACK aggressiveness policy.
    VoiceNackMode,
    /// Desktop theme mode (`system`/`light`/`dark`).
    ThemeMode,
    /// Desktop theme contrast tier (`standard`/`high`).
    ThemeContrast,
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
            Self::VoiceDenoiseEnabled => "voice.denoise_enabled",
            Self::VoicePartialPlayOnTimeout => "voice.partial_play_on_timeout",
            Self::VoiceFecMode => "voice.fec_mode",
            Self::VoiceNackMode => "voice.nack_mode",
            Self::ThemeMode => "theme.mode",
            Self::ThemeContrast => "theme.contrast",
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
            "voice.denoise_enabled" => Self::VoiceDenoiseEnabled,
            "voice.partial_play_on_timeout" => Self::VoicePartialPlayOnTimeout,
            "voice.fec_mode" => Self::VoiceFecMode,
            "voice.nack_mode" => Self::VoiceNackMode,
            "theme.mode" => Self::ThemeMode,
            "theme.contrast" => Self::ThemeContrast,
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
            SettingKey::VoiceDenoiseEnabled,
            SettingKey::VoicePartialPlayOnTimeout,
            SettingKey::VoiceFecMode,
            SettingKey::VoiceNackMode,
            SettingKey::ThemeMode,
            SettingKey::ThemeContrast,
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
    /// Boolean toggle. `set_str` accepts `"true"`/`"false"` (case-insensitive),
    /// plus `"1"`/`"0"`, `"yes"`/`"no"`, `"on"`/`"off"`.
    Bool,
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

impl Default for VoiceCodecKind {
    fn default() -> Self {
        Self::from_id(DEFAULT_VOICE_CODEC).expect("DEFAULT_VOICE_CODEC is always a valid id")
    }
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

impl Default for OpusBandwidthKind {
    fn default() -> Self {
        Self::from_id(DEFAULT_OPUS_BANDWIDTH).expect("DEFAULT_OPUS_BANDWIDTH is always a valid id")
    }
}

// ---------------------------------------------------------------------------
// FEC parity policy (sender-side)
// ---------------------------------------------------------------------------

/// Sender-side parity policy. Resolved to an actual `parity_count` at
/// `build_message` time via [`VoiceFecMode::resolve`], which takes the
/// destination (broadcast vs unicast) and the modem preset into account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceFecMode {
    /// Pick parity by destination + preset (the recommended default).
    Auto,
    /// Disable FEC. Rely on NACK retransmits (or accept partial on
    /// broadcast). Saves airtime upfront, costs round-trips on loss.
    Off,
    /// ~10 % of `total_data`, capped to [`MAX_PARITY_PER_MESSAGE`].
    Light,
    /// ~25 % of `total_data`.
    Medium,
    /// ~50 % of `total_data`. Recommended for broadcast or high-loss
    /// long-range presets where NACK round-trips are expensive.
    Heavy,
}

impl VoiceFecMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => VOICE_FEC_MODE_AUTO,
            Self::Off => VOICE_FEC_MODE_OFF,
            Self::Light => VOICE_FEC_MODE_LIGHT,
            Self::Medium => VOICE_FEC_MODE_MEDIUM,
            Self::Heavy => VOICE_FEC_MODE_HEAVY,
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Some(match s {
            VOICE_FEC_MODE_AUTO => Self::Auto,
            VOICE_FEC_MODE_OFF => Self::Off,
            VOICE_FEC_MODE_LIGHT => Self::Light,
            VOICE_FEC_MODE_MEDIUM => Self::Medium,
            VOICE_FEC_MODE_HEAVY => Self::Heavy,
            _ => return None,
        })
    }

    /// Resolve this mode + context to a concrete `parity_count`.
    ///
    /// `broadcast` flips the `Auto` branch to 50 % since broadcast cannot
    /// rely on NACK recovery. `preset` is consulted only for `Auto` on
    /// unicast; manual modes ignore it.
    ///
    /// Returned value is clamped to
    /// `[0, min(total_data, MAX_PARITY_PER_MESSAGE, MAX_TOTAL_SHARDS - total_data)]`
    /// so callers never need to re-check the protocol cap — including the
    /// Reed-Solomon `data + parity <= 256` limit, which `total_data` near the
    /// `MAX_CHUNKS_PER_MESSAGE` ceiling would otherwise blow past on a 50 %
    /// (broadcast / Heavy) parity ratio.
    pub fn resolve(
        self,
        broadcast: bool,
        preset: Option<crate::voice::ModemPreset>,
        total_data: usize,
    ) -> u8 {
        use crate::voice::{MAX_PARITY_PER_MESSAGE, MAX_TOTAL_SHARDS, ModemPreset};
        let pct = match (self, broadcast) {
            (Self::Off, _) => 0,
            (Self::Light, _) => 10,
            (Self::Medium, _) => 25,
            (Self::Heavy, _) => 50,
            (Self::Auto, true) => 50,
            (Self::Auto, false) => match preset {
                Some(
                    ModemPreset::LongFast
                    | ModemPreset::LongModerate
                    | ModemPreset::LongSlow
                    | ModemPreset::VeryLongSlow,
                ) => 33,
                Some(ModemPreset::MediumFast | ModemPreset::MediumSlow) => 20,
                Some(ModemPreset::ShortTurbo | ModemPreset::ShortFast | ModemPreset::ShortSlow) => {
                    0
                }
                None => 20, // unknown preset: assume mid-range
            },
        };
        let raw = (total_data * pct).div_ceil(100);
        let cap = total_data
            .min(MAX_PARITY_PER_MESSAGE)
            .min(MAX_TOTAL_SHARDS.saturating_sub(total_data))
            .min(u8::MAX as usize);
        raw.min(cap) as u8
    }
}

pub fn voice_fec_mode_to_id(m: VoiceFecMode) -> &'static str {
    m.id()
}

pub fn voice_fec_mode_from_id(s: &str) -> Option<VoiceFecMode> {
    VoiceFecMode::from_id(s)
}

impl Default for VoiceFecMode {
    fn default() -> Self {
        Self::from_id(DEFAULT_VOICE_FEC_MODE).expect("DEFAULT_VOICE_FEC_MODE is always a valid id")
    }
}

// ---------------------------------------------------------------------------
// NACK aggressiveness policy (receiver-side)
// ---------------------------------------------------------------------------

/// Receiver-side NACK policy. Resolved to concrete `(nack_window,
/// backoff_pow, max_nack_rounds)` at the assembler tick via
/// [`VoiceNackMode::resolve`].
///
/// Broadcast messages are **always** treated as `Off` regardless of this
/// setting — the assembler short-circuits the NACK emission branch when
/// `state.to == VoiceDestination::Broadcast` so the override is silently
/// ignored. This keeps a chatty channel from being flooded with NACKs
/// from every listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceNackMode {
    /// Pick window/backoff/cap by preset (the recommended default).
    Auto,
    /// Never emit a NACK. Receiver falls back to FEC + partial-on-timeout.
    Off,
    /// Long quiet windows, fast (3^n) backoff growth, low round cap.
    /// Use when retransmits are expensive (slow presets, low SNR).
    Conservative,
    /// Short quiet windows, slow (2^n) backoff growth, high round cap.
    /// Use when retransmits are cheap (fast presets, good SNR).
    Aggressive,
}

/// Concrete parameters resolved from a [`VoiceNackMode`] + preset.
/// `nack_window` is the base quiet period; the assembler multiplies by
/// `backoff_base.pow(round)` for the effective window of round `n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NackParams {
    pub nack_window: std::time::Duration,
    /// Backoff base used as `nack_window × base^round`. `2` doubles each
    /// round; `3` triples. Special value `0` means "NACK disabled" — the
    /// assembler skips emission entirely.
    pub backoff_base: u32,
    pub max_nack_rounds: u16,
}

impl NackParams {
    /// `Off`: NACK suppressed; backoff/round cap fields are inert. The
    /// assembler checks `backoff_base == 0` and short-circuits.
    pub const fn disabled() -> Self {
        Self {
            nack_window: std::time::Duration::from_secs(3_600),
            backoff_base: 0,
            max_nack_rounds: 0,
        }
    }
}

impl VoiceNackMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => VOICE_NACK_MODE_AUTO,
            Self::Off => VOICE_NACK_MODE_OFF,
            Self::Conservative => VOICE_NACK_MODE_CONSERVATIVE,
            Self::Aggressive => VOICE_NACK_MODE_AGGRESSIVE,
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Some(match s {
            VOICE_NACK_MODE_AUTO => Self::Auto,
            VOICE_NACK_MODE_OFF => Self::Off,
            VOICE_NACK_MODE_CONSERVATIVE => Self::Conservative,
            VOICE_NACK_MODE_AGGRESSIVE => Self::Aggressive,
            _ => return None,
        })
    }

    /// Resolve to concrete `NackParams`. `preset` is consulted for
    /// `Auto`; manual modes use a flat policy independent of preset.
    pub fn resolve(self, preset: Option<crate::voice::ModemPreset>) -> NackParams {
        use crate::voice::ModemPreset;
        use std::time::Duration;

        let pacing = preset
            .map(ModemPreset::pacing)
            .unwrap_or_else(ModemPreset::fallback_pacing);

        match self {
            Self::Off => NackParams::disabled(),
            Self::Conservative => NackParams {
                nack_window: (pacing * 4).max(Duration::from_millis(4_000)),
                backoff_base: 3,
                max_nack_rounds: 200,
            },
            Self::Aggressive => NackParams {
                nack_window: Duration::from_millis(1_500),
                backoff_base: 2,
                max_nack_rounds: 800,
            },
            Self::Auto => match preset {
                Some(
                    ModemPreset::ShortTurbo
                    | ModemPreset::ShortFast
                    | ModemPreset::ShortSlow
                    | ModemPreset::MediumFast,
                ) => NackParams {
                    nack_window: Duration::from_millis(1_500),
                    backoff_base: 2,
                    max_nack_rounds: 800,
                },
                Some(ModemPreset::MediumSlow | ModemPreset::LongFast) => NackParams {
                    nack_window: Duration::from_millis(3_000),
                    backoff_base: 2,
                    max_nack_rounds: 400,
                },
                Some(
                    ModemPreset::LongModerate | ModemPreset::LongSlow | ModemPreset::VeryLongSlow,
                )
                | None => NackParams {
                    nack_window: (pacing * 4).max(Duration::from_millis(4_000)),
                    backoff_base: 3,
                    max_nack_rounds: 200,
                },
            },
        }
    }
}

pub fn voice_nack_mode_to_id(m: VoiceNackMode) -> &'static str {
    m.id()
}

pub fn voice_nack_mode_from_id(s: &str) -> Option<VoiceNackMode> {
    VoiceNackMode::from_id(s)
}

impl Default for VoiceNackMode {
    fn default() -> Self {
        Self::from_id(DEFAULT_VOICE_NACK_MODE)
            .expect("DEFAULT_VOICE_NACK_MODE is always a valid id")
    }
}

// ---------------------------------------------------------------------------
// Theme mode + contrast (desktop GUI only — Android has its own theme path)
// ---------------------------------------------------------------------------

/// Typed mirror of the `theme.mode` string id. Drives egui's
/// [`ThemePreference`] on the desktop GUI; the bridge layer ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeModeKind {
    /// Follow the host (OS / desktop environment) preference.
    System,
    /// Force light scheme regardless of host preference.
    Light,
    /// Force dark scheme regardless of host preference.
    Dark,
}

impl ThemeModeKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::System => THEME_MODE_SYSTEM,
            Self::Light => THEME_MODE_LIGHT,
            Self::Dark => THEME_MODE_DARK,
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Some(match s {
            THEME_MODE_SYSTEM => Self::System,
            THEME_MODE_LIGHT => Self::Light,
            THEME_MODE_DARK => Self::Dark,
            _ => return None,
        })
    }
}

pub fn theme_mode_kind_to_id(k: ThemeModeKind) -> &'static str {
    k.id()
}

pub fn theme_mode_kind_from_id(s: &str) -> Option<ThemeModeKind> {
    ThemeModeKind::from_id(s)
}

impl Default for ThemeModeKind {
    fn default() -> Self {
        Self::from_id(DEFAULT_THEME_MODE).expect("DEFAULT_THEME_MODE is always a valid id")
    }
}

/// Typed mirror of the `theme.contrast` string id. `Standard` uses the
/// M3 TonalSpot palette; `High` opts into the HighContrast variant that
/// mirrors the firmware `meshtastic-device-ui` theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeContrastKind {
    Standard,
    High,
}

impl ThemeContrastKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Standard => THEME_CONTRAST_STANDARD,
            Self::High => THEME_CONTRAST_HIGH,
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Some(match s {
            THEME_CONTRAST_STANDARD => Self::Standard,
            THEME_CONTRAST_HIGH => Self::High,
            _ => return None,
        })
    }
}

pub fn theme_contrast_kind_to_id(k: ThemeContrastKind) -> &'static str {
    k.id()
}

pub fn theme_contrast_kind_from_id(s: &str) -> Option<ThemeContrastKind> {
    ThemeContrastKind::from_id(s)
}

impl Default for ThemeContrastKind {
    fn default() -> Self {
        Self::from_id(DEFAULT_THEME_CONTRAST).expect("DEFAULT_THEME_CONTRAST is always a valid id")
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use crate::voice::ModemPreset;

    #[test]
    fn fec_auto_broadcast_uses_50pct_regardless_of_preset() {
        for preset in [
            None,
            Some(ModemPreset::ShortFast),
            Some(ModemPreset::LongSlow),
        ] {
            let p = VoiceFecMode::Auto.resolve(true, preset, 10);
            assert_eq!(
                p, 5,
                "broadcast must always force 50% parity (preset={preset:?})"
            );
        }
    }

    #[test]
    fn fec_auto_unicast_scales_with_preset() {
        // Short-range presets get no FEC: NACK round-trips are cheap.
        assert_eq!(
            VoiceFecMode::Auto.resolve(false, Some(ModemPreset::ShortFast), 10),
            0
        );
        // Medium presets get ~20%.
        assert_eq!(
            VoiceFecMode::Auto.resolve(false, Some(ModemPreset::MediumFast), 10),
            2
        );
        // Long-range presets get ~33%.
        assert_eq!(
            VoiceFecMode::Auto.resolve(false, Some(ModemPreset::LongFast), 10),
            4
        );
        assert_eq!(
            VoiceFecMode::Auto.resolve(false, Some(ModemPreset::VeryLongSlow), 10),
            4
        );
    }

    #[test]
    fn fec_manual_modes_ignore_preset() {
        let preset = Some(ModemPreset::ShortFast);
        assert_eq!(VoiceFecMode::Off.resolve(false, preset, 10), 0);
        assert_eq!(VoiceFecMode::Light.resolve(false, preset, 10), 1);
        assert_eq!(VoiceFecMode::Medium.resolve(false, preset, 10), 3); // div_ceil(10*25, 100) = 3
        assert_eq!(VoiceFecMode::Heavy.resolve(false, preset, 10), 5);
    }

    #[test]
    fn fec_resolve_clamps_to_total_data() {
        // Heavy on a tiny message must not exceed total_data.
        assert_eq!(VoiceFecMode::Heavy.resolve(false, None, 2), 1);
        assert_eq!(VoiceFecMode::Heavy.resolve(false, None, 1), 1);
    }

    #[test]
    fn fec_resolve_caps_at_rs_sum_limit() {
        use crate::voice::MAX_TOTAL_SHARDS;
        // At the max total_data, the RS `data + parity <= 256` limit leaves
        // room for only one parity shard, even though Heavy asks for 50%.
        let p = VoiceFecMode::Heavy.resolve(false, None, 255);
        assert_eq!(p, 1);
        assert!(255 + p as usize <= MAX_TOTAL_SHARDS);
    }

    #[test]
    fn fec_auto_broadcast_never_exceeds_rs_limit() {
        use crate::voice::MAX_TOTAL_SHARDS;
        // Auto broadcast forces 50% parity — the worst case for the RS sum.
        // No total_data may produce data + parity > 256.
        for total_data in 1..=255usize {
            let p = VoiceFecMode::Auto.resolve(true, None, total_data) as usize;
            assert!(
                total_data + p <= MAX_TOTAL_SHARDS,
                "total_data={total_data} parity={p} exceeds RS limit"
            );
        }
        // Spot-check the historical failure point: 171 data + 50% would be 86
        // (= 257), now clamped to 85.
        assert_eq!(VoiceFecMode::Auto.resolve(true, None, 171), 85);
    }

    #[test]
    fn nack_off_disables_emission_via_backoff_base_zero() {
        let p = VoiceNackMode::Off.resolve(Some(ModemPreset::LongFast));
        assert_eq!(
            p.backoff_base, 0,
            "Off mode must signal disabled via backoff_base == 0"
        );
    }

    #[test]
    fn nack_aggressive_uses_2x_backoff_and_short_window() {
        let p = VoiceNackMode::Aggressive.resolve(Some(ModemPreset::LongSlow));
        assert_eq!(p.backoff_base, 2);
        assert_eq!(p.nack_window, std::time::Duration::from_millis(1_500));
        assert!(p.max_nack_rounds >= 400);
    }

    #[test]
    fn nack_conservative_uses_3x_backoff_and_long_window() {
        let p = VoiceNackMode::Conservative.resolve(Some(ModemPreset::ShortFast));
        assert_eq!(p.backoff_base, 3);
        assert!(p.nack_window >= std::time::Duration::from_secs(4));
    }

    #[test]
    fn nack_auto_picks_aggressive_on_fast_preset() {
        let p = VoiceNackMode::Auto.resolve(Some(ModemPreset::ShortFast));
        assert_eq!(p.backoff_base, 2);
        assert!(p.nack_window <= std::time::Duration::from_millis(2_000));
    }

    #[test]
    fn nack_auto_picks_conservative_on_slow_preset() {
        let p = VoiceNackMode::Auto.resolve(Some(ModemPreset::LongSlow));
        assert_eq!(p.backoff_base, 3);
    }
}

// ---------------------------------------------------------------------------
// Settings API
// ---------------------------------------------------------------------------

/// Whether loading the settings file succeeded or produced degraded defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadStatus {
    /// File was missing (first run) or parsed successfully.
    Clean,
    /// File existed but could not be read (e.g. EACCES) or failed to parse.
    /// Defaults are in use; the broken file will be backed up before the next
    /// successful persist so it is never silently overwritten.
    Degraded,
}

/// Read and parse a settings file from disk.
///
/// Returns defaults + `LoadStatus::Clean` when the file is missing (first run)
/// or on a successful parse. Returns defaults + `LoadStatus::Degraded` when
/// the file exists but cannot be read (e.g. EACCES) or fails to parse. In
/// the degraded case a warning is logged; the on-disk file is left intact so
/// the user can fix it manually.
fn read_settings_at(path: &std::path::Path) -> (AppSettings, LoadStatus) {
    let s = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (AppSettings::default(), LoadStatus::Clean);
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "config.toml could not be read; using defaults",
            );
            return (AppSettings::default(), LoadStatus::Degraded);
        }
    };
    match toml::from_str(&s) {
        Ok(data) => (data, LoadStatus::Clean),
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "config.toml failed to parse; using defaults until fixed or replaced",
            );
            (AppSettings::default(), LoadStatus::Degraded)
        }
    }
}

/// Central settings facade. Cheap to clone (it's an `Arc` internally
/// via [`SettingsApi::open`]); listeners and persistence are shared.
pub struct SettingsApi {
    inner: RwLock<AppSettings>,
    /// Persistence path. `None` means in-memory only (tests, headless).
    path: RwLock<Option<PathBuf>>,
    listeners: Mutex<Vec<Arc<dyn SettingsListener>>>,
    /// Serializes [`Self::persist`] so concurrent setters (this API is shared
    /// via `Arc` across the GUI and runtime) can't interleave writes into a
    /// half-formed file.
    persist_lock: Mutex<()>,
    /// Set when the on-disk config could not be read or parsed. In degraded
    /// mode `persist()` renames the broken file to `config.toml.broken-<ts>`
    /// before writing so the user can recover their settings manually.
    degraded: std::sync::atomic::AtomicBool,
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
        let (data, status) = match path.as_ref() {
            Some(p) => read_settings_at(p),
            None => (AppSettings::default(), LoadStatus::Clean),
        };
        Arc::new(Self {
            inner: RwLock::new(data),
            path: RwLock::new(path),
            listeners: Mutex::new(Vec::new()),
            persist_lock: Mutex::new(()),
            degraded: std::sync::atomic::AtomicBool::new(status == LoadStatus::Degraded),
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

    /// Reload from disk, discarding any in-memory edits. Clears the degraded
    /// flag if the file now parses successfully.
    pub fn reload(&self) {
        let p = self.path.read().clone();
        let (data, status) = match p {
            Some(p) => read_settings_at(&p),
            None => (AppSettings::default(), LoadStatus::Clean),
        };
        self.degraded.store(
            status == LoadStatus::Degraded,
            std::sync::atomic::Ordering::Relaxed,
        );
        *self.inner.write() = data;
        for k in SettingKey::all() {
            self.notify(*k);
        }
    }

    /// Returns `true` when the settings file existed but could not be read or
    /// parsed at last open/reload. In degraded mode the first successful
    /// [`persist`] will rename the broken file to `config.toml.broken-<ts>`
    /// before writing so it can be recovered manually.
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(std::sync::atomic::Ordering::Relaxed)
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
        VoiceCodecKind::from_id(self.inner.read().voice_codec()).unwrap_or_default()
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
        OpusBandwidthKind::from_id(self.inner.read().voice_opus_bandwidth()).unwrap_or_default()
    }

    pub fn voice_denoise_enabled(&self) -> bool {
        self.inner.read().voice_denoise_enabled()
    }

    pub fn voice_fec_mode(&self) -> VoiceFecMode {
        VoiceFecMode::from_id(self.inner.read().voice_fec_mode()).unwrap_or_default()
    }

    pub fn voice_nack_mode(&self) -> VoiceNackMode {
        VoiceNackMode::from_id(self.inner.read().voice_nack_mode()).unwrap_or_default()
    }

    pub fn theme_mode(&self) -> ThemeModeKind {
        ThemeModeKind::from_id(self.inner.read().theme_mode()).unwrap_or_default()
    }

    pub fn theme_contrast(&self) -> ThemeContrastKind {
        ThemeContrastKind::from_id(self.inner.read().theme_contrast()).unwrap_or_default()
    }

    /// Convenience: resolve `voice.codec` + per-codec mode to the
    /// `VoiceCodecParam` the voice protocol layer wants.
    ///
    /// Takes a single lock snapshot so codec and parameter are always
    /// consistent even if a setter fires between two separate reads.
    pub fn voice_codec_for_protocol(&self) -> VoiceCodecParam {
        let inner = self.inner.read();
        match VoiceCodecKind::from_id(inner.voice_codec()).unwrap_or_default() {
            VoiceCodecKind::Opus => VoiceCodecParam {
                codec: VoiceCodec::Opus,
                param: inner.voice_opus_bitrate_kbps(),
            },
            VoiceCodecKind::Codec2 => VoiceCodecParam {
                codec: VoiceCodec::Codec2,
                param: inner.voice_codec2_mode(),
            },
            VoiceCodecKind::AmrNb => VoiceCodecParam {
                codec: VoiceCodec::AmrNb,
                param: inner.voice_amrnb_mode(),
            },
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

    pub fn set_voice_denoise_enabled(&self, enabled: bool) -> SettingsResult<()> {
        self.inner.write().voice_denoise_enabled = Some(enabled);
        self.persist_and_notify(SettingKey::VoiceDenoiseEnabled)
    }

    pub fn voice_partial_play_on_timeout(&self) -> bool {
        self.inner.read().voice_partial_play_on_timeout()
    }

    pub fn set_voice_partial_play_on_timeout(&self, enabled: bool) -> SettingsResult<()> {
        self.inner.write().voice_partial_play_on_timeout = Some(enabled);
        self.persist_and_notify(SettingKey::VoicePartialPlayOnTimeout)
    }

    pub fn set_voice_fec_mode(&self, mode: VoiceFecMode) -> SettingsResult<()> {
        self.inner.write().voice_fec_mode = Some(mode.id().to_string());
        self.persist_and_notify(SettingKey::VoiceFecMode)
    }

    pub fn set_voice_nack_mode(&self, mode: VoiceNackMode) -> SettingsResult<()> {
        self.inner.write().voice_nack_mode = Some(mode.id().to_string());
        self.persist_and_notify(SettingKey::VoiceNackMode)
    }

    pub fn set_theme_mode(&self, mode: ThemeModeKind) -> SettingsResult<()> {
        self.inner.write().theme_mode = Some(mode.id().to_string());
        self.persist_and_notify(SettingKey::ThemeMode)
    }

    pub fn set_theme_contrast(&self, contrast: ThemeContrastKind) -> SettingsResult<()> {
        self.inner.write().theme_contrast = Some(contrast.id().to_string());
        self.persist_and_notify(SettingKey::ThemeContrast)
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
                SettingKey::VoiceDenoiseEnabled => g.voice_denoise_enabled = None,
                SettingKey::VoicePartialPlayOnTimeout => g.voice_partial_play_on_timeout = None,
                SettingKey::VoiceFecMode => g.voice_fec_mode = None,
                SettingKey::VoiceNackMode => g.voice_nack_mode = None,
                SettingKey::ThemeMode => g.theme_mode = None,
                SettingKey::ThemeContrast => g.theme_contrast = None,
            }
        }
        self.persist_and_notify(key)
    }

    /// Reset every field at once.
    pub fn reset_all(&self) -> SettingsResult<()> {
        *self.inner.write() = AppSettings::default();
        let result = self.persist();
        for k in SettingKey::all() {
            self.notify(*k);
        }
        result
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
            SettingKey::VoiceDenoiseEnabled => self.voice_denoise_enabled().to_string(),
            SettingKey::VoicePartialPlayOnTimeout => {
                self.voice_partial_play_on_timeout().to_string()
            }
            SettingKey::VoiceFecMode => self.voice_fec_mode().id().to_string(),
            SettingKey::VoiceNackMode => self.voice_nack_mode().id().to_string(),
            SettingKey::ThemeMode => self.theme_mode().id().to_string(),
            SettingKey::ThemeContrast => self.theme_contrast().id().to_string(),
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
            SettingKey::VoiceDenoiseEnabled => {
                let b = parse_bool(key, value)?;
                self.set_voice_denoise_enabled(b)
            }
            SettingKey::VoicePartialPlayOnTimeout => {
                let b = parse_bool(key, value)?;
                self.set_voice_partial_play_on_timeout(b)
            }
            SettingKey::VoiceFecMode => {
                let mode = VoiceFecMode::from_id(value).ok_or_else(|| SettingsError::Invalid {
                    key: key.id(),
                    value: value.to_string(),
                    reason: format!(
                        "expected one of {VOICE_FEC_MODE_AUTO}, {VOICE_FEC_MODE_OFF}, {VOICE_FEC_MODE_LIGHT}, {VOICE_FEC_MODE_MEDIUM}, {VOICE_FEC_MODE_HEAVY}"
                    ),
                })?;
                self.set_voice_fec_mode(mode)
            }
            SettingKey::VoiceNackMode => {
                let mode = VoiceNackMode::from_id(value).ok_or_else(|| SettingsError::Invalid {
                    key: key.id(),
                    value: value.to_string(),
                    reason: format!(
                        "expected one of {VOICE_NACK_MODE_AUTO}, {VOICE_NACK_MODE_OFF}, {VOICE_NACK_MODE_CONSERVATIVE}, {VOICE_NACK_MODE_AGGRESSIVE}"
                    ),
                })?;
                self.set_voice_nack_mode(mode)
            }
            SettingKey::ThemeMode => {
                let mode = ThemeModeKind::from_id(value).ok_or_else(|| SettingsError::Invalid {
                    key: key.id(),
                    value: value.to_string(),
                    reason: format!(
                        "expected one of {THEME_MODE_SYSTEM}, {THEME_MODE_LIGHT}, {THEME_MODE_DARK}"
                    ),
                })?;
                self.set_theme_mode(mode)
            }
            SettingKey::ThemeContrast => {
                let contrast =
                    ThemeContrastKind::from_id(value).ok_or_else(|| SettingsError::Invalid {
                        key: key.id(),
                        value: value.to_string(),
                        reason: format!(
                            "expected one of {THEME_CONTRAST_STANDARD}, {THEME_CONTRAST_HIGH}"
                        ),
                    })?;
                self.set_theme_contrast(contrast)
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
            SettingKey::VoiceDenoiseEnabled => (
                "Noise suppression",
                "Run captured audio through an RNNoise-based denoiser before the encoder. Reduces stationary background noise (fans, HVAC, keyboard) at the cost of ~10 ms latency. No effect on builds without the `denoise` feature.",
                SettingKind::Bool,
                DEFAULT_VOICE_DENOISE_ENABLED.to_string(),
            ),
            SettingKey::VoicePartialPlayOnTimeout => (
                "Play partial voice on timeout",
                "When a voice message never completes within the reassembly window, play back whatever chunks did arrive (silence padded for the rest) instead of dropping the message.",
                SettingKind::Bool,
                DEFAULT_VOICE_PARTIAL_PLAY_ON_TIMEOUT.to_string(),
            ),
            SettingKey::VoiceFecMode => (
                "FEC parity policy (sender)",
                "Reed-Solomon parity overhead. `auto` picks 50% for broadcast, 33% for long-range unicast, 20% medium, 0% short. Manual modes set a flat percentage of `total_data`.",
                SettingKind::Enum {
                    variants: vec![
                        VOICE_FEC_MODE_AUTO,
                        VOICE_FEC_MODE_OFF,
                        VOICE_FEC_MODE_LIGHT,
                        VOICE_FEC_MODE_MEDIUM,
                        VOICE_FEC_MODE_HEAVY,
                    ],
                },
                DEFAULT_VOICE_FEC_MODE.to_string(),
            ),
            SettingKey::VoiceNackMode => (
                "NACK aggressiveness (receiver)",
                "Quiet-window, backoff exponent, and round cap for the NACK loop. `auto` picks by modem preset. Broadcast messages always behave as `off` regardless — they cannot use NACK reliably.",
                SettingKind::Enum {
                    variants: vec![
                        VOICE_NACK_MODE_AUTO,
                        VOICE_NACK_MODE_OFF,
                        VOICE_NACK_MODE_CONSERVATIVE,
                        VOICE_NACK_MODE_AGGRESSIVE,
                    ],
                },
                DEFAULT_VOICE_NACK_MODE.to_string(),
            ),
            SettingKey::ThemeMode => (
                "Desktop theme mode",
                "`system` follows the host OS preference; `light` / `dark` pin the scheme. Applies immediately on the desktop GUI.",
                SettingKind::Enum {
                    variants: vec![THEME_MODE_SYSTEM, THEME_MODE_LIGHT, THEME_MODE_DARK],
                },
                DEFAULT_THEME_MODE.to_string(),
            ),
            SettingKey::ThemeContrast => (
                "Desktop theme contrast",
                "`standard` uses the M3 TonalSpot palette; `high` mirrors the meshtastic-device-ui firmware look (near-black on near-white, and its inverse) as an a11y option.",
                SettingKind::Enum {
                    variants: vec![THEME_CONTRAST_STANDARD, THEME_CONTRAST_HIGH],
                },
                DEFAULT_THEME_CONTRAST.to_string(),
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
        let result = self.persist();
        self.notify(key);
        result
    }

    fn persist(&self) -> SettingsResult<()> {
        // Hold the persist lock across the whole snapshot-serialize-write so
        // two concurrent setters can't interleave and produce malformed TOML.
        let _guard = self.persist_lock.lock();
        let path = self.path.read().clone();
        let Some(path) = path else {
            return Ok(()); // in-memory only
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(&*self.inner.read())
            .map_err(|e| SettingsError::Io(std::io::Error::other(e)))?;
        // In degraded mode the broken config is still on disk. Rename it to a
        // dated backup before writing so it is never silently clobbered. If the
        // rename itself fails (e.g. EACCES on a read-only filesystem) return an
        // error without touching anything: the broken file is safer than nothing.
        if self.degraded.load(std::sync::atomic::Ordering::Relaxed) && path.exists() {
            let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
            let backup = path.with_extension(format!("toml.broken-{ts}"));
            std::fs::rename(&path, &backup)?;
        }
        // PID-suffixed tmp name: if two processes (e.g. a running GUI and a
        // CLI invocation) both write config they won't stomp on the same temp
        // file. The persist lock already serialises concurrent writers within
        // one process.
        let tmp = path.with_extension(format!("toml.tmp.{}", std::process::id()));
        let result: SettingsResult<()> = (|| {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(body.as_bytes())?;
            // Flush kernel write buffers to the device before renaming. A
            // kernel crash between write and rename without this could leave
            // the renamed file empty on remount. We deliberately omit a
            // parent-directory fsync: it would block on slow media and is
            // only needed for directory-entry durability on power loss, which
            // is an acceptable trade-off for a settings file.
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp); // best-effort cleanup
        } else {
            self.degraded
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
        result
    }

    fn notify(&self, key: SettingKey) {
        let listeners: Vec<_> = self.listeners.lock().iter().cloned().collect();
        for l in listeners {
            l.on_change(key);
        }
    }
}

fn parse_u32(key: SettingKey, value: &str) -> SettingsResult<u32> {
    value
        .trim()
        .parse::<u32>()
        .map_err(|e| SettingsError::Invalid {
            key: key.id(),
            value: value.to_string(),
            reason: e.to_string(),
        })
}

fn parse_bool(key: SettingKey, value: &str) -> SettingsResult<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(SettingsError::Invalid {
            key: key.id(),
            value: value.to_string(),
            reason: "expected true/false (or 1/0, yes/no, on/off)".to_string(),
        }),
    }
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

    #[test]
    fn missing_file_is_not_degraded() {
        let path = std::env::temp_dir().join(format!(
            "voicetastic-degraded-test-{}-missing.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let api = SettingsApi::open_at(Some(path));
        assert!(
            !api.is_degraded(),
            "missing file must be Clean not Degraded"
        );
    }

    #[test]
    fn garbage_file_is_degraded_then_cleared_on_persist() {
        let path = std::env::temp_dir().join(format!(
            "voicetastic-degraded-test-{}-garbage.toml",
            std::process::id()
        ));
        std::fs::write(&path, b"this is not valid toml }{").unwrap();
        let api = SettingsApi::open_at(Some(path.clone()));
        assert!(api.is_degraded(), "unreadable file must be Degraded");
        // A setter should back up the broken file and write a valid one.
        api.set_voice_codec(VoiceCodecKind::Opus).unwrap();
        assert!(
            !api.is_degraded(),
            "degraded flag must clear after successful persist"
        );
        // The new config file must be valid.
        let api2 = SettingsApi::open_at(Some(path.clone()));
        assert!(!api2.is_degraded());
        assert_eq!(api2.voice_codec(), VoiceCodecKind::Opus);
        // A .broken-* backup must exist alongside the new file.
        let dir = path.parent().unwrap();
        let has_backup = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                let name = e.file_name();
                name.to_string_lossy().contains(".broken-")
            });
        assert!(has_backup, "a .broken-<ts> backup file must exist");
        // Cleanup: remove both the config and any backup.
        let _ = std::fs::remove_file(&path);
        for e in std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()) {
            if e.file_name().to_string_lossy().contains(".broken-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    #[test]
    fn reload_clears_degraded_after_manual_fix() {
        let path = std::env::temp_dir().join(format!(
            "voicetastic-degraded-test-{}-fix.toml",
            std::process::id()
        ));
        std::fs::write(&path, b"not valid toml").unwrap();
        let api = SettingsApi::open_at(Some(path.clone()));
        assert!(api.is_degraded());
        // Simulate user manually fixing the file.
        std::fs::write(&path, b"").unwrap(); // empty = valid defaults TOML
        api.reload();
        assert!(
            !api.is_degraded(),
            "reload with valid file must clear degraded"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persist_leaves_no_tmp_sibling() {
        let config = std::env::temp_dir().join(format!(
            "voicetastic-atomic-test-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&config);
        let api = SettingsApi::open_at(Some(config.clone()));
        api.set_voice_codec(VoiceCodecKind::Opus).unwrap();
        let dir = config.parent().unwrap();
        let stem = config.file_stem().unwrap().to_string_lossy();
        let has_tmp = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                let name = e.file_name();
                let s = name.to_string_lossy();
                s.starts_with(stem.as_ref()) && s.contains(".tmp")
            });
        assert!(
            !has_tmp,
            "no .tmp sibling should remain after a successful persist"
        );
        let _ = std::fs::remove_file(&config);
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

    /// Listeners must fire even when persist() returns an error (e.g. when the
    /// settings directory is read-only). Memory is already updated before persist
    /// is called; skipping notify would leave live state desynced from memory.
    #[test]
    #[cfg(unix)]
    fn listener_fires_even_when_persist_fails() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir =
            std::env::temp_dir().join(format!("voicetastic-notify-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let api = SettingsApi::open_at(Some(path));
        // Initial write to establish the file.
        api.set_voice_codec(VoiceCodecKind::Opus).unwrap();
        // Make the directory read-only so future writes fail.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o444)).unwrap();
        let c = Arc::new(Counter(Default::default()));
        api.subscribe(c.clone());
        let result = api.set_voice_codec(VoiceCodecKind::AmrNb);
        // Restore permissions before any assertion so cleanup always runs.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            result.is_err(),
            "persist should have failed on read-only dir"
        );
        assert_eq!(
            c.0.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "listener must fire even when persist errors"
        );
        assert_eq!(
            api.voice_codec(),
            VoiceCodecKind::AmrNb,
            "memory must be updated"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn theme_settings_round_trip() {
        let api = SettingsApi::in_memory();
        // Defaults: Dark / Standard (matches the historical startup pin).
        assert_eq!(api.theme_mode(), ThemeModeKind::Dark);
        assert_eq!(api.theme_contrast(), ThemeContrastKind::Standard);

        api.set_theme_mode(ThemeModeKind::Light).unwrap();
        api.set_theme_contrast(ThemeContrastKind::High).unwrap();
        assert_eq!(api.theme_mode(), ThemeModeKind::Light);
        assert_eq!(api.theme_contrast(), ThemeContrastKind::High);

        // Stringified access (CLI / bridge surface).
        assert_eq!(api.get_str(SettingKey::ThemeMode), THEME_MODE_LIGHT);
        assert_eq!(api.get_str(SettingKey::ThemeContrast), THEME_CONTRAST_HIGH);

        // Reset clears the override; effective value returns to the default.
        api.reset(SettingKey::ThemeMode).unwrap();
        assert_eq!(api.theme_mode(), ThemeModeKind::Dark);
    }

    #[test]
    fn theme_set_str_rejects_garbage() {
        let api = SettingsApi::in_memory();
        let err = api
            .set_str(SettingKey::ThemeMode, "neon")
            .expect_err("garbage theme mode must be rejected");
        assert!(matches!(err, SettingsError::Invalid { .. }));
        let err = api
            .set_str(SettingKey::ThemeContrast, "extra-high")
            .expect_err("garbage contrast must be rejected");
        assert!(matches!(err, SettingsError::Invalid { .. }));
    }

    /// Default impls for the six enum types must match the corresponding
    /// DEFAULT_* constants so fallback values can't drift from constants.
    #[test]
    fn fallback_defaults_match_data_constants() {
        use super::super::data::{
            DEFAULT_OPUS_BANDWIDTH, DEFAULT_THEME_CONTRAST, DEFAULT_THEME_MODE,
            DEFAULT_VOICE_CODEC, DEFAULT_VOICE_FEC_MODE, DEFAULT_VOICE_NACK_MODE,
        };
        assert_eq!(VoiceCodecKind::default().id(), DEFAULT_VOICE_CODEC);
        assert_eq!(OpusBandwidthKind::default().id(), DEFAULT_OPUS_BANDWIDTH);
        assert_eq!(VoiceFecMode::default().id(), DEFAULT_VOICE_FEC_MODE);
        assert_eq!(VoiceNackMode::default().id(), DEFAULT_VOICE_NACK_MODE);
        assert_eq!(ThemeModeKind::default().id(), DEFAULT_THEME_MODE);
        assert_eq!(ThemeContrastKind::default().id(), DEFAULT_THEME_CONTRAST);
    }

    /// voice_codec_for_protocol must return codec and param from the same
    /// consistent snapshot (single lock acquisition).
    #[test]
    fn voice_codec_for_protocol_returns_consistent_pair() {
        let api = SettingsApi::in_memory();
        let pair = api.voice_codec_for_protocol();
        assert_eq!(pair.codec, VoiceCodec::AmrNb);
        assert_eq!(pair.param, api.voice_amrnb_mode());
    }

    /// parse_u32 (via set_str) must accept whitespace-padded values just
    /// like parse_bool does.
    #[test]
    fn set_str_trims_whitespace_for_u32() {
        let api = SettingsApi::in_memory();
        api.set_str(SettingKey::VoiceMaxDurationSecs, "  30  ")
            .expect("set_str must accept padded integer");
        assert_eq!(api.voice_max_secs(), 30);
    }
}
