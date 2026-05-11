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
    SettingDescriptor, SettingKey, SettingKind, SettingsApi, SettingsError, SettingsListener,
    SettingsResult, VoiceCodecKind, voice_codec_kind_from_id, voice_codec_kind_to_id,
};
pub use data::{
    AppSettings, CODEC2_MODE_1200, CODEC2_MODE_1300, CODEC2_MODE_1400, CODEC2_MODE_1600,
    CODEC2_MODE_2400, CODEC2_MODE_3200, DEFAULT_CODEC2_MODE, DEFAULT_REASSEMBLY_TIMEOUT_SECS,
    DEFAULT_VOICE_CODEC, DEFAULT_VOICE_MAX_SECS, REASSEMBLY_TIMEOUT_LOWER_SECS,
    REASSEMBLY_TIMEOUT_UPPER_SECS, VOICE_CODEC_CODEC2, VOICE_CODEC_OPUS, VOICE_MAX_SECS_UPPER,
};
