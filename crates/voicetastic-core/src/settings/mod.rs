//! Persistent client-side settings (last device, voice prefs, ...).
//!
//! Two layers live here:
//!
//! - [`data`] — the on-disk schema ([`AppSettings`]) plus the value
//!   constants and clamping helpers. This is the de-facto wire format
//!   for the config file and remains stable.
//! - [`api`] — [`SettingsApi`], the public facade every front-end
//!   (GUI, CLI, Android bridge) is expected to drive. Typed
//!   getters / setters, validation, persistence, listener notifications
//!   and a descriptor list for generic UIs all live there.
//!
//! Front-ends should depend on [`SettingsApi`]; [`AppSettings`] is
//! retained as the persisted shape and as a public field-snapshot for
//! callers that need raw access.

pub mod api;
pub mod data;

pub use api::{
    NackParams, OpusBandwidthKind, SettingDescriptor, SettingKey, SettingKind, SettingsApi,
    SettingsError, SettingsListener, SettingsResult, ThemeContrastKind, ThemeModeKind,
    VoiceCodecKind, VoiceCodecParam, VoiceFecMode, VoiceNackMode, opus_bandwidth_kind_from_id,
    opus_bandwidth_kind_to_id, theme_contrast_kind_from_id, theme_contrast_kind_to_id,
    theme_mode_kind_from_id, theme_mode_kind_to_id, voice_codec_kind_from_id,
    voice_codec_kind_to_id, voice_fec_mode_from_id, voice_fec_mode_to_id, voice_nack_mode_from_id,
    voice_nack_mode_to_id,
};
pub use data::{
    AMRNB_MODE_475, AMRNB_MODE_515, AMRNB_MODE_590, AMRNB_MODE_670, AMRNB_MODE_740, AMRNB_MODE_795,
    AMRNB_MODE_1020, AMRNB_MODE_1220, AppSettings, CODEC2_MODE_1200, CODEC2_MODE_1300,
    CODEC2_MODE_1400, CODEC2_MODE_1600, CODEC2_MODE_2400, CODEC2_MODE_3200, DEFAULT_AMRNB_MODE,
    DEFAULT_CODEC2_MODE, DEFAULT_OPUS_BANDWIDTH, DEFAULT_OPUS_BITRATE_KBPS,
    DEFAULT_REASSEMBLY_TIMEOUT_SECS, DEFAULT_THEME_CONTRAST, DEFAULT_THEME_MODE,
    DEFAULT_VOICE_CODEC, DEFAULT_VOICE_DENOISE_ENABLED, DEFAULT_VOICE_FEC_MODE,
    DEFAULT_VOICE_MAX_SECS, DEFAULT_VOICE_NACK_MODE, DEFAULT_VOICE_PARTIAL_PLAY_ON_TIMEOUT,
    OPUS_BANDWIDTH_NARROW, OPUS_BANDWIDTH_WIDE,
    OPUS_BITRATE_KBPS_MAX, OPUS_BITRATE_KBPS_MIN, REASSEMBLY_TIMEOUT_LOWER_SECS,
    REASSEMBLY_TIMEOUT_UPPER_SECS, THEME_CONTRAST_HIGH, THEME_CONTRAST_STANDARD, THEME_MODE_DARK,
    THEME_MODE_LIGHT, THEME_MODE_SYSTEM, VOICE_CODEC_AMRNB, VOICE_CODEC_CODEC2, VOICE_CODEC_OPUS,
    VOICE_FEC_MODE_AUTO, VOICE_FEC_MODE_HEAVY, VOICE_FEC_MODE_LIGHT, VOICE_FEC_MODE_MEDIUM,
    VOICE_FEC_MODE_OFF, VOICE_MAX_SECS_UPPER, VOICE_NACK_MODE_AGGRESSIVE, VOICE_NACK_MODE_AUTO,
    VOICE_NACK_MODE_CONSERVATIVE, VOICE_NACK_MODE_OFF,
};
