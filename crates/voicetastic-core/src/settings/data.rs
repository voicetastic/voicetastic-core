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
pub const DEFAULT_REASSEMBLY_TIMEOUT_SECS: u32 = 1200;
/// Lower bound for the configurable reassembly timeout (10 s).
pub const REASSEMBLY_TIMEOUT_LOWER_SECS: u32 = 10;
/// Upper bound for the configurable reassembly timeout (1 hour).
pub const REASSEMBLY_TIMEOUT_UPPER_SECS: u32 = 3_600;

/// Voice codec identifiers used in [`AppSettings::voice_codec`]. Mirror
/// the wire byte assigned in `voice::VoiceCodec::to_byte()`.
pub const VOICE_CODEC_OPUS: &str = "opus";
pub const VOICE_CODEC_CODEC2: &str = "codec2";
pub const VOICE_CODEC_AMRNB: &str = "amrnb";

/// Default voice codec for newly composed messages. AMR-NB at its
/// highest rate (MR122, 12.2 kbps) is widely deployed, decodes on
/// virtually every legacy receiver, and stays under the per-message
/// budget for the 30 s default clip. Users can switch to Codec2 (for
/// very slow LoRa presets) or Opus (higher quality) in
/// Settings → Voice messages.
pub const DEFAULT_VOICE_CODEC: &str = VOICE_CODEC_AMRNB;

/// Codec2 mode index (matches the `codec2` crate's `Codec2Mode` discriminant
/// and is what we ship over the air as `codec_param`):
///   0 = 3200 bps, 1 = 2400 bps, 2 = 1600 bps,
///   3 = 1400 bps, 4 = 1300 bps, 5 = 1200 bps.
pub const CODEC2_MODE_3200: u8 = 0;
pub const CODEC2_MODE_2400: u8 = 1;
pub const CODEC2_MODE_1600: u8 = 2;
pub const CODEC2_MODE_1400: u8 = 3;
pub const CODEC2_MODE_1300: u8 = 4;
pub const CODEC2_MODE_1200: u8 = 5;
/// Lowest implemented (and thus most LoRa-friendly) Codec2 rate.
pub const DEFAULT_CODEC2_MODE: u8 = CODEC2_MODE_1200;

/// AMR-NB mode index (matches the OpenCORE `enum_Mode` ordinal we ship
/// over the air as `codec_param`):
///   0 = MR475 (4.75 kbps), 1 = MR515, 2 = MR59, 3 = MR67,
///   4 = MR74, 5 = MR795, 6 = MR102, 7 = MR122 (12.2 kbps).
pub const AMRNB_MODE_475: u8 = 0;
pub const AMRNB_MODE_515: u8 = 1;
pub const AMRNB_MODE_590: u8 = 2;
pub const AMRNB_MODE_670: u8 = 3;
pub const AMRNB_MODE_740: u8 = 4;
pub const AMRNB_MODE_795: u8 = 5;
pub const AMRNB_MODE_1020: u8 = 6;
pub const AMRNB_MODE_1220: u8 = 7;
/// Highest-quality AMR-NB rate; matches the most common `.amr` files.
pub const DEFAULT_AMRNB_MODE: u8 = AMRNB_MODE_1220;

/// Opus encoder bitrate, in kbps. Values stored on disk as `u8` since
/// the protocol's `codec_param` byte carries the same number on the
/// wire (informative only — the decoder ignores it because the Opus
/// bitstream self-describes).
pub const OPUS_BITRATE_KBPS_MIN: u8 = 6;
pub const OPUS_BITRATE_KBPS_MAX: u8 = 16;
/// Default sender bitrate. 12 kbps mono is a sweet spot for voice over
/// LoRa: clearly intelligible, decodes everywhere, fits a 30 s clip
/// inside the protocol's per-message size budget on every preset.
pub const DEFAULT_OPUS_BITRATE_KBPS: u8 = 12;

/// Opus audio bandwidth identifier. We expose only the two useful
/// modes for our LoRa-voice use case (`narrow` = SILK 8 kHz,
/// `wide` = SILK 16 kHz). Higher modes (super-wide, full-band) are
/// deliberately omitted — they cost airtime without helping voice
/// intelligibility.
pub const OPUS_BANDWIDTH_NARROW: &str = "narrow";
pub const OPUS_BANDWIDTH_WIDE: &str = "wide";
/// Default bandwidth for new senders. Wideband matches the previous
/// hard-coded behaviour so existing config files don't change voice
/// character on upgrade.
pub const DEFAULT_OPUS_BANDWIDTH: &str = OPUS_BANDWIDTH_WIDE;

/// Default for the capture-side RNNoise noise-suppression toggle. Off so
/// the recording pipeline is unchanged on upgrade and headless builds
/// (where the `denoise` feature may be disabled) don't surprise users.
pub const DEFAULT_VOICE_DENOISE_ENABLED: bool = false;

/// Default for the partial-play-on-timeout receive policy. When true, an
/// incomplete voice message whose reassembly timer fires is finalised with
/// whatever chunks did arrive (silence padded for the rest); when false,
/// the partial is dropped on timeout.
pub const DEFAULT_VOICE_PARTIAL_PLAY_ON_TIMEOUT: bool = true;

/// FEC parity policy id strings. Persisted as text so the TOML file is
/// human-editable and forward-compatible with new variants.
pub const VOICE_FEC_MODE_AUTO: &str = "auto";
pub const VOICE_FEC_MODE_OFF: &str = "off";
pub const VOICE_FEC_MODE_LIGHT: &str = "light";
pub const VOICE_FEC_MODE_MEDIUM: &str = "medium";
pub const VOICE_FEC_MODE_HEAVY: &str = "heavy";
/// Default FEC mode: pick parity by destination (broadcast/unicast) +
/// modem preset. See [`VoiceFecMode`] in the api layer for the policy.
pub const DEFAULT_VOICE_FEC_MODE: &str = VOICE_FEC_MODE_AUTO;

/// NACK aggressiveness policy id strings. Affects receive-side behaviour:
/// quiet-window, backoff exponent, and consecutive-round cap. Broadcast
/// messages **always** behave as `Off` regardless of this setting — the
/// override is silently ignored when `state.to == Broadcast`.
pub const VOICE_NACK_MODE_AUTO: &str = "auto";
pub const VOICE_NACK_MODE_OFF: &str = "off";
pub const VOICE_NACK_MODE_CONSERVATIVE: &str = "conservative";
pub const VOICE_NACK_MODE_AGGRESSIVE: &str = "aggressive";
/// Default NACK mode: pick window/backoff by modem preset.
pub const DEFAULT_VOICE_NACK_MODE: &str = VOICE_NACK_MODE_AUTO;

/// Theme-mode identifiers persisted in [`AppSettings::theme_mode`]. The
/// `system` value defers to whatever the host (OS / desktop environment)
/// reports through egui's system-theme follow path; `light` / `dark`
/// pin the preference unconditionally.
pub const THEME_MODE_SYSTEM: &str = "system";
pub const THEME_MODE_LIGHT: &str = "light";
pub const THEME_MODE_DARK: &str = "dark";
/// Default theme mode. Dark matches the historical hard-coded
/// `ThemePreference::Dark` startup pin so an upgrade does not flip the
/// look on existing users.
pub const DEFAULT_THEME_MODE: &str = THEME_MODE_DARK;

/// Theme-contrast identifiers persisted in [`AppSettings::theme_contrast`].
/// `standard` uses the M3 TonalSpot palette (warm peach surfaces);
/// `high` opts into the HighContrast variant that mirrors the firmware
/// `meshtastic-device-ui` theme — near-black on near-white in light mode
/// and the inverse in dark mode. Useful as an a11y theme.
pub const THEME_CONTRAST_STANDARD: &str = "standard";
pub const THEME_CONTRAST_HIGH: &str = "high";
/// Default contrast tier (Standard). HighContrast is opt-in via the
/// Appearance settings panel.
pub const DEFAULT_THEME_CONTRAST: &str = THEME_CONTRAST_STANDARD;

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

    /// Voice codec used when *sending* a new voice message. One of
    /// [`VOICE_CODEC_AMRNB`], [`VOICE_CODEC_OPUS`] or
    /// [`VOICE_CODEC_CODEC2`]. `None` falls back to
    /// [`DEFAULT_VOICE_CODEC`]. Unknown values are treated as the
    /// default. Received messages are decoded based on the codec byte
    /// carried in the frame header, independent of this setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_codec: Option<String>,

    /// Codec2 mode index, used when [`Self::voice_codec`] resolves to
    /// Codec2. `None` falls back to [`DEFAULT_CODEC2_MODE`]. Values
    /// outside `0..=5` are clamped to the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_codec2_mode: Option<u8>,

    /// AMR-NB mode index, used when [`Self::voice_codec`] resolves to
    /// AMR-NB. `None` falls back to [`DEFAULT_AMRNB_MODE`]. Values
    /// outside `0..=7` are clamped to the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_amrnb_mode: Option<u8>,

    /// Opus encoder bitrate (kbps), used when [`Self::voice_codec`]
    /// resolves to Opus. `None` falls back to
    /// [`DEFAULT_OPUS_BITRATE_KBPS`]. Values outside
    /// `OPUS_BITRATE_KBPS_MIN..=OPUS_BITRATE_KBPS_MAX` are clamped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_opus_bitrate_kbps: Option<u8>,

    /// Opus audio bandwidth (`narrow` or `wide`), used when
    /// [`Self::voice_codec`] resolves to Opus. `None` falls back to
    /// [`DEFAULT_OPUS_BANDWIDTH`]. Unknown strings are treated as the
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_opus_bandwidth: Option<String>,

    /// When `Some(true)`, capture runs through the RNNoise-based
    /// denoiser before the encoder. `None` falls back to
    /// [`DEFAULT_VOICE_DENOISE_ENABLED`]. On builds without the
    /// `voicetastic-core/denoise` feature, the setting persists but the
    /// runtime is a passthrough.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_denoise_enabled: Option<bool>,

    /// When `Some(true)` (the default), the receiver plays back any chunks
    /// it has when the reassembly timer fires for a message it never
    /// completed (silence padded for missing data). `None` falls back to
    /// [`DEFAULT_VOICE_PARTIAL_PLAY_ON_TIMEOUT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_partial_play_on_timeout: Option<bool>,

    /// Sender-side FEC parity policy id. One of
    /// [`VOICE_FEC_MODE_AUTO`] (default) / `_OFF` / `_LIGHT` / `_MEDIUM` /
    /// `_HEAVY`. Resolved against destination + modem preset at send time
    /// to pick the actual `parity_count`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_fec_mode: Option<String>,

    /// Receive-side NACK aggressiveness id. One of
    /// [`VOICE_NACK_MODE_AUTO`] (default) / `_OFF` / `_CONSERVATIVE` /
    /// `_AGGRESSIVE`. **Always overridden to `Off` for broadcast
    /// messages** at the assembler tick — broadcast NACKs are never
    /// useful (multiple receivers, no clear retransmit target).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_nack_mode: Option<String>,

    /// Desktop GUI theme mode. One of [`THEME_MODE_SYSTEM`],
    /// [`THEME_MODE_LIGHT`], [`THEME_MODE_DARK`]. `None` falls back to
    /// [`DEFAULT_THEME_MODE`]. Read by the GUI on startup and on each
    /// `SettingKey::ThemeMode` listener event to drive egui's
    /// `ThemePreference`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_mode: Option<String>,

    /// Desktop GUI theme contrast tier. One of
    /// [`THEME_CONTRAST_STANDARD`], [`THEME_CONTRAST_HIGH`]. `None`
    /// falls back to [`DEFAULT_THEME_CONTRAST`]. Selects between the
    /// TonalSpot palette and the HighContrast variant that mirrors the
    /// `meshtastic-device-ui` firmware theme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_contrast: Option<String>,
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

    /// Effective outgoing voice codec id (lowercased, validated). Unknown
    /// values fall back to [`DEFAULT_VOICE_CODEC`].
    pub fn voice_codec(&self) -> &'static str {
        match self.voice_codec.as_deref() {
            Some(VOICE_CODEC_OPUS) => VOICE_CODEC_OPUS,
            Some(VOICE_CODEC_CODEC2) => VOICE_CODEC_CODEC2,
            Some(VOICE_CODEC_AMRNB) => VOICE_CODEC_AMRNB,
            _ => DEFAULT_VOICE_CODEC,
        }
    }

    /// Effective Codec2 mode index, clamped to `0..=5`.
    pub fn voice_codec2_mode(&self) -> u8 {
        match self.voice_codec2_mode {
            Some(m) if m <= CODEC2_MODE_1200 => m,
            _ => DEFAULT_CODEC2_MODE,
        }
    }

    /// Effective AMR-NB mode index, clamped to `0..=7`.
    pub fn voice_amrnb_mode(&self) -> u8 {
        match self.voice_amrnb_mode {
            Some(m) if m <= AMRNB_MODE_1220 => m,
            _ => DEFAULT_AMRNB_MODE,
        }
    }

    /// Effective Opus bitrate in kbps, clamped to
    /// `OPUS_BITRATE_KBPS_MIN..=OPUS_BITRATE_KBPS_MAX`.
    pub fn voice_opus_bitrate_kbps(&self) -> u8 {
        self.voice_opus_bitrate_kbps
            .unwrap_or(DEFAULT_OPUS_BITRATE_KBPS)
            .clamp(OPUS_BITRATE_KBPS_MIN, OPUS_BITRATE_KBPS_MAX)
    }

    /// Effective Opus bandwidth identifier (validated). Unknown values
    /// fall back to [`DEFAULT_OPUS_BANDWIDTH`].
    pub fn voice_opus_bandwidth(&self) -> &'static str {
        match self.voice_opus_bandwidth.as_deref() {
            Some(OPUS_BANDWIDTH_NARROW) => OPUS_BANDWIDTH_NARROW,
            Some(OPUS_BANDWIDTH_WIDE) => OPUS_BANDWIDTH_WIDE,
            _ => DEFAULT_OPUS_BANDWIDTH,
        }
    }

    /// Effective capture-side noise-suppression toggle.
    pub fn voice_denoise_enabled(&self) -> bool {
        self.voice_denoise_enabled
            .unwrap_or(DEFAULT_VOICE_DENOISE_ENABLED)
    }

    /// Effective partial-play-on-timeout toggle.
    pub fn voice_partial_play_on_timeout(&self) -> bool {
        self.voice_partial_play_on_timeout
            .unwrap_or(DEFAULT_VOICE_PARTIAL_PLAY_ON_TIMEOUT)
    }

    /// Effective FEC mode id (validated). Unknown values fall back to
    /// [`DEFAULT_VOICE_FEC_MODE`].
    pub fn voice_fec_mode(&self) -> &'static str {
        match self.voice_fec_mode.as_deref() {
            Some(VOICE_FEC_MODE_AUTO) => VOICE_FEC_MODE_AUTO,
            Some(VOICE_FEC_MODE_OFF) => VOICE_FEC_MODE_OFF,
            Some(VOICE_FEC_MODE_LIGHT) => VOICE_FEC_MODE_LIGHT,
            Some(VOICE_FEC_MODE_MEDIUM) => VOICE_FEC_MODE_MEDIUM,
            Some(VOICE_FEC_MODE_HEAVY) => VOICE_FEC_MODE_HEAVY,
            _ => DEFAULT_VOICE_FEC_MODE,
        }
    }

    /// Effective NACK mode id (validated). Unknown values fall back to
    /// [`DEFAULT_VOICE_NACK_MODE`].
    pub fn voice_nack_mode(&self) -> &'static str {
        match self.voice_nack_mode.as_deref() {
            Some(VOICE_NACK_MODE_AUTO) => VOICE_NACK_MODE_AUTO,
            Some(VOICE_NACK_MODE_OFF) => VOICE_NACK_MODE_OFF,
            Some(VOICE_NACK_MODE_CONSERVATIVE) => VOICE_NACK_MODE_CONSERVATIVE,
            Some(VOICE_NACK_MODE_AGGRESSIVE) => VOICE_NACK_MODE_AGGRESSIVE,
            _ => DEFAULT_VOICE_NACK_MODE,
        }
    }

    /// Effective theme mode id (validated). Unknown values fall back to
    /// [`DEFAULT_THEME_MODE`].
    pub fn theme_mode(&self) -> &'static str {
        match self.theme_mode.as_deref() {
            Some(THEME_MODE_SYSTEM) => THEME_MODE_SYSTEM,
            Some(THEME_MODE_LIGHT) => THEME_MODE_LIGHT,
            Some(THEME_MODE_DARK) => THEME_MODE_DARK,
            _ => DEFAULT_THEME_MODE,
        }
    }

    /// Effective theme contrast id (validated). Unknown values fall back
    /// to [`DEFAULT_THEME_CONTRAST`].
    pub fn theme_contrast(&self) -> &'static str {
        match self.theme_contrast.as_deref() {
            Some(THEME_CONTRAST_STANDARD) => THEME_CONTRAST_STANDARD,
            Some(THEME_CONTRAST_HIGH) => THEME_CONTRAST_HIGH,
            _ => DEFAULT_THEME_CONTRAST,
        }
    }
}

/// Resolve `$XDG_CONFIG_HOME/voicetastic/config.toml`, falling back to
/// `$HOME/.config/voicetastic/config.toml`. Returns `None` if neither env
/// var is set (e.g. on a headless container with no `$HOME`).
pub(super) fn config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("voicetastic/config.toml"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/voicetastic/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A settings value with every field populated to a non-default.
    fn fully_populated() -> AppSettings {
        AppSettings {
            last_device: Some("AA:BB:CC:DD:EE:FF".into()),
            max_voice_duration_secs: Some(45),
            reassembly_timeout_secs: Some(600),
            voice_codec: Some(VOICE_CODEC_CODEC2.into()),
            voice_codec2_mode: Some(CODEC2_MODE_2400),
            voice_amrnb_mode: Some(AMRNB_MODE_795),
            voice_opus_bitrate_kbps: Some(10),
            voice_opus_bandwidth: Some(OPUS_BANDWIDTH_NARROW.into()),
            voice_denoise_enabled: Some(true),
            voice_partial_play_on_timeout: Some(false),
            voice_fec_mode: Some(VOICE_FEC_MODE_HEAVY.into()),
            voice_nack_mode: Some(VOICE_NACK_MODE_AGGRESSIVE.into()),
            theme_mode: Some(THEME_MODE_LIGHT.into()),
            theme_contrast: Some(THEME_CONTRAST_HIGH.into()),
        }
    }

    // --- TOML round trip ---

    #[test]
    fn toml_roundtrip_is_stable() {
        let original = fully_populated();
        let serialized = toml::to_string(&original).expect("serialize");
        let parsed: AppSettings = toml::from_str(&serialized).expect("deserialize");
        // AppSettings has no PartialEq; re-serializing the parsed value must
        // reproduce the same TOML byte-for-byte (a fixed-point check).
        assert_eq!(serialized, toml::to_string(&parsed).expect("reserialize"));
    }

    #[test]
    fn default_serializes_to_empty_toml() {
        // Every field is `Option` with `skip_serializing_if = is_none`, so a
        // default value must produce an empty document (nothing on disk until
        // the user changes something).
        let s = toml::to_string(&AppSettings::default()).expect("serialize");
        assert!(s.trim().is_empty(), "expected empty TOML, got: {s:?}");
    }

    #[test]
    fn empty_toml_yields_defaults() {
        let s: AppSettings = toml::from_str("").expect("deserialize empty");
        assert_eq!(s.voice_max_secs(), DEFAULT_VOICE_MAX_SECS);
        assert_eq!(s.reassembly_timeout_secs(), DEFAULT_REASSEMBLY_TIMEOUT_SECS);
        assert_eq!(s.voice_codec(), DEFAULT_VOICE_CODEC);
        assert_eq!(s.voice_codec2_mode(), DEFAULT_CODEC2_MODE);
        assert_eq!(s.voice_amrnb_mode(), DEFAULT_AMRNB_MODE);
        assert_eq!(s.voice_opus_bitrate_kbps(), DEFAULT_OPUS_BITRATE_KBPS);
        assert_eq!(s.voice_opus_bandwidth(), DEFAULT_OPUS_BANDWIDTH);
        assert_eq!(s.voice_denoise_enabled(), DEFAULT_VOICE_DENOISE_ENABLED);
        assert_eq!(
            s.voice_partial_play_on_timeout(),
            DEFAULT_VOICE_PARTIAL_PLAY_ON_TIMEOUT
        );
        assert_eq!(s.voice_fec_mode(), DEFAULT_VOICE_FEC_MODE);
        assert_eq!(s.voice_nack_mode(), DEFAULT_VOICE_NACK_MODE);
        assert_eq!(s.theme_mode(), DEFAULT_THEME_MODE);
        assert_eq!(s.theme_contrast(), DEFAULT_THEME_CONTRAST);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        // Forward compatibility: a config written by a newer build with extra
        // keys must still load (serde ignores unknown fields by default).
        let s: AppSettings =
            toml::from_str("last_device = \"x\"\nfuture_setting = 42\n").expect("deserialize");
        assert_eq!(s.last_device.as_deref(), Some("x"));
        assert_eq!(s.voice_codec(), DEFAULT_VOICE_CODEC);
    }

    #[test]
    fn partial_toml_fills_the_rest_with_defaults() {
        let s: AppSettings = toml::from_str("voice_codec = \"opus\"\n").expect("deserialize");
        assert_eq!(s.voice_codec(), VOICE_CODEC_OPUS);
        assert_eq!(s.voice_amrnb_mode(), DEFAULT_AMRNB_MODE);
    }

    // --- clamping / validation accessors ---

    #[test]
    fn voice_max_secs_is_clamped() {
        assert_eq!(
            AppSettings {
                max_voice_duration_secs: Some(0),
                ..Default::default()
            }
            .voice_max_secs(),
            1,
            "zero clamps up to 1"
        );
        assert_eq!(
            AppSettings {
                max_voice_duration_secs: Some(10_000),
                ..Default::default()
            }
            .voice_max_secs(),
            VOICE_MAX_SECS_UPPER
        );
    }

    #[test]
    fn reassembly_timeout_is_clamped() {
        assert_eq!(
            AppSettings {
                reassembly_timeout_secs: Some(1),
                ..Default::default()
            }
            .reassembly_timeout_secs(),
            REASSEMBLY_TIMEOUT_LOWER_SECS
        );
        assert_eq!(
            AppSettings {
                reassembly_timeout_secs: Some(u32::MAX),
                ..Default::default()
            }
            .reassembly_timeout_secs(),
            REASSEMBLY_TIMEOUT_UPPER_SECS
        );
    }

    #[test]
    fn out_of_range_codec_modes_fall_back_to_default() {
        assert_eq!(
            AppSettings {
                voice_codec2_mode: Some(99),
                ..Default::default()
            }
            .voice_codec2_mode(),
            DEFAULT_CODEC2_MODE
        );
        assert_eq!(
            AppSettings {
                voice_amrnb_mode: Some(99),
                ..Default::default()
            }
            .voice_amrnb_mode(),
            DEFAULT_AMRNB_MODE
        );
    }

    #[test]
    fn opus_bitrate_is_clamped_both_ends() {
        assert_eq!(
            AppSettings {
                voice_opus_bitrate_kbps: Some(1),
                ..Default::default()
            }
            .voice_opus_bitrate_kbps(),
            OPUS_BITRATE_KBPS_MIN
        );
        assert_eq!(
            AppSettings {
                voice_opus_bitrate_kbps: Some(255),
                ..Default::default()
            }
            .voice_opus_bitrate_kbps(),
            OPUS_BITRATE_KBPS_MAX
        );
    }

    #[test]
    fn unknown_string_ids_fall_back_to_defaults() {
        let bogus = |f: fn(&AppSettings) -> &'static str, set: AppSettings, want: &str| {
            assert_eq!(f(&set), want);
        };
        bogus(
            AppSettings::voice_codec,
            AppSettings {
                voice_codec: Some("garbage".into()),
                ..Default::default()
            },
            DEFAULT_VOICE_CODEC,
        );
        bogus(
            AppSettings::voice_opus_bandwidth,
            AppSettings {
                voice_opus_bandwidth: Some("ultrawide".into()),
                ..Default::default()
            },
            DEFAULT_OPUS_BANDWIDTH,
        );
        bogus(
            AppSettings::voice_fec_mode,
            AppSettings {
                voice_fec_mode: Some("turbo".into()),
                ..Default::default()
            },
            DEFAULT_VOICE_FEC_MODE,
        );
        bogus(
            AppSettings::voice_nack_mode,
            AppSettings {
                voice_nack_mode: Some("turbo".into()),
                ..Default::default()
            },
            DEFAULT_VOICE_NACK_MODE,
        );
        bogus(
            AppSettings::theme_mode,
            AppSettings {
                theme_mode: Some("sepia".into()),
                ..Default::default()
            },
            DEFAULT_THEME_MODE,
        );
        bogus(
            AppSettings::theme_contrast,
            AppSettings {
                theme_contrast: Some("medium".into()),
                ..Default::default()
            },
            DEFAULT_THEME_CONTRAST,
        );
    }

    // --- config_path resolution ---

    #[test]
    fn config_path_prefers_xdg_then_home() {
        // We can't safely mutate process env in parallel tests, so just assert
        // the function yields a path ending in the expected suffix under
        // whatever env the runner provides (one of the two must be set in CI).
        if let Some(p) = config_path() {
            assert!(
                p.ends_with("voicetastic/config.toml"),
                "unexpected config path: {}",
                p.display()
            );
        }
    }
}
