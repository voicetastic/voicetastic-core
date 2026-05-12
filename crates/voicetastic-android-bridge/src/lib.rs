// SPDX-License-Identifier: MIT
//
//! UniFFI bridge: exposes `voicetastic-core`'s voice protocol to Kotlin/Android.
//!
//! The bridge is intentionally narrow:
//! - `build_message` / `build_nack` / `random_message_id` / `detect_version`
//!   as free functions.
//! - `VoiceAssembler` as a stateful interface.
//!
//! All UDL <-> Rust shape conversions live in this file; the rest of
//! `voicetastic-core` is unchanged.
//!
//! Threading: `VoiceAssembler` is `Send + Sync` upstream (uses
//! `parking_lot::Mutex`); UniFFI wraps it in `Arc<Self>` so Kotlin can
//! share it across coroutines safely.

use std::time::Duration;

use voicetastic_core::settings as s;
use voicetastic_core::voice as v;

// -----------------------------------------------------------------------------
// Errors
// -----------------------------------------------------------------------------

/// UDL-facing error type. Variant names match the UDL `[Error] enum`.
/// We don't carry payloads here — the on-rails diagnostic comes from
/// `Display`, which UniFFI surfaces to Kotlin as the exception message.
#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("packet too short")]
    TooShort,
    #[error("packet too large")]
    TooLarge,
    #[error("unsupported protocol version")]
    BadVersion,
    #[error("reserved type_flags bit set")]
    ReservedFlagSet,
    #[error("reserved packet_type")]
    ReservedPacketType,
    #[error("message_id must be non-zero")]
    ZeroMessageId,
    #[error("invalid totalData")]
    BadTotal,
    #[error("parity_count exceeds MAX_PARITY_PER_MESSAGE")]
    TooMuchParity,
    #[error("chunk_index out of range")]
    BadIndex,
    #[error("audio too large for one message")]
    AudioTooLarge,
    #[error("chunk_size below minimum")]
    ChunkTooSmall,
    #[error("chunk_size exceeds maximum body size")]
    ChunkTooLarge,
    #[error("body length does not match established chunk_size")]
    BodyLenMismatch,
    #[error("AES-GCM authentication failed")]
    BadTag,
    #[error("body too short for encryption envelope")]
    BodyTooShortForEnv,
    #[error("NACK frames must not have the encrypted bit set")]
    EncryptedNack,
    #[error("NACK frame body too short")]
    NackTooShort,
    #[error("Reed-Solomon error")]
    Fec,
    #[error("codec mismatch within message")]
    CodecMismatch,
    #[error("total_data mismatch within message")]
    TotalMismatch,
    #[error("stream_seq mismatch within message")]
    StreamSeqMismatch,
    #[error("parity_count decreased within message")]
    ParityCountDecrease,
    #[error("NACK frame chunk_index must be 0")]
    BadNackIndex,
    #[error("unknown codec byte")]
    UnknownCodec,
    #[error("codec is not supported by this receiver")]
    UnsupportedCodec,
    #[error("(from, message_id) is on the recently-completed blacklist")]
    Blacklisted,
    #[error("per-sender in-flight cap reached")]
    PerSenderCap,
    #[error("encrypted frame received but no channel PSK is configured")]
    EncryptedNoPsk,
    #[error("`from` field is not a valid !hex8 node id (required for encrypted frames)")]
    BadFromForEncrypted,
    #[error("OS RNG unavailable")]
    Rng,
}

impl From<v::VoiceError> for VoiceError {
    fn from(e: v::VoiceError) -> Self {
        // Keep this match exhaustive so the compiler errors when upstream
        // adds a new variant — that's the whole point of the bridge.
        match e {
            v::VoiceError::TooShort { .. } => Self::TooShort,
            v::VoiceError::TooLarge { .. } => Self::TooLarge,
            v::VoiceError::BadVersion(_) => Self::BadVersion,
            v::VoiceError::ReservedFlagSet(_) => Self::ReservedFlagSet,
            v::VoiceError::ReservedPacketType => Self::ReservedPacketType,
            v::VoiceError::ZeroMessageId => Self::ZeroMessageId,
            v::VoiceError::BadTotal(_) => Self::BadTotal,
            v::VoiceError::TooMuchParity(_) => Self::TooMuchParity,
            v::VoiceError::BadIndex { .. } => Self::BadIndex,
            v::VoiceError::AudioTooLarge { .. } => Self::AudioTooLarge,
            v::VoiceError::ChunkTooSmall(_) => Self::ChunkTooSmall,
            v::VoiceError::ChunkTooLarge { .. } => Self::ChunkTooLarge,
            v::VoiceError::BodyLenMismatch { .. } => Self::BodyLenMismatch,
            v::VoiceError::BadTag => Self::BadTag,
            v::VoiceError::BodyTooShortForEnv(_) => Self::BodyTooShortForEnv,
            v::VoiceError::EncryptedNack => Self::EncryptedNack,
            v::VoiceError::NackTooShort => Self::NackTooShort,
            v::VoiceError::Fec(_) => Self::Fec,
            v::VoiceError::CodecMismatch { .. } => Self::CodecMismatch,
            v::VoiceError::TotalMismatch { .. } => Self::TotalMismatch,
            v::VoiceError::StreamSeqMismatch { .. } => Self::StreamSeqMismatch,
            v::VoiceError::ParityCountDecrease { .. } => Self::ParityCountDecrease,
            v::VoiceError::BadNackIndex(_) => Self::BadNackIndex,
            v::VoiceError::UnknownCodec(_) => Self::UnknownCodec,
            v::VoiceError::UnsupportedCodec(_) => Self::UnsupportedCodec,
            v::VoiceError::Blacklisted => Self::Blacklisted,
            v::VoiceError::PerSenderCap(_) => Self::PerSenderCap,
            v::VoiceError::EncryptedNoPsk => Self::EncryptedNoPsk,
            v::VoiceError::BadFromForEncrypted(_) => Self::BadFromForEncrypted,
            v::VoiceError::Rng(_) => Self::Rng,
        }
    }
}

// -----------------------------------------------------------------------------
// Codec
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceCodec {
    AmrNb,
    Opus,
    PcmS16Le,
    Codec2,
    Unknown { raw: u8 },
}

impl From<v::VoiceCodec> for VoiceCodec {
    fn from(c: v::VoiceCodec) -> Self {
        match c {
            v::VoiceCodec::AmrNb => Self::AmrNb,
            v::VoiceCodec::Opus => Self::Opus,
            v::VoiceCodec::PcmS16Le => Self::PcmS16Le,
            v::VoiceCodec::Codec2 => Self::Codec2,
            v::VoiceCodec::Unknown(raw) => Self::Unknown { raw },
        }
    }
}

impl From<VoiceCodec> for v::VoiceCodec {
    fn from(c: VoiceCodec) -> Self {
        match c {
            VoiceCodec::AmrNb => Self::AmrNb,
            VoiceCodec::Opus => Self::Opus,
            VoiceCodec::PcmS16Le => Self::PcmS16Le,
            VoiceCodec::Codec2 => Self::Codec2,
            VoiceCodec::Unknown { raw } => Self::Unknown(raw),
        }
    }
}

// -----------------------------------------------------------------------------
// Build (sender) side
// -----------------------------------------------------------------------------

#[derive(Debug)]
pub struct BuildConfig {
    pub message_id: u32,
    pub stream_seq: u8,
    pub codec: VoiceCodec,
    pub codec_param: u8,
    pub chunk_size: u32,
    pub parity_count: u8,
    pub last_in_stream: bool,
    pub channel_psk: Option<Vec<u8>>,
    pub from_node_num: u32,
}

#[derive(Debug)]
pub struct EncodedMessage {
    pub frames: Vec<Vec<u8>>,
    pub total_data: u8,
    pub parity_count: u8,
}

impl From<v::EncodedMessage> for EncodedMessage {
    fn from(m: v::EncodedMessage) -> Self {
        Self {
            frames: m.frames,
            total_data: m.total_data,
            parity_count: m.parity_count,
        }
    }
}

pub fn random_message_id() -> u32 {
    // Bridge keeps the infallible signature: Android always has a working
    // OS RNG, and panicking here would only happen if /dev/urandom is
    // unavailable at process start — a configuration error worth surfacing
    // loudly. Desktop callers should use `voicetastic_core::voice::random_message_id`
    // directly to get the fallible variant.
    v::random_message_id().expect("OS RNG unavailable")
}

pub fn detect_version(payload: Vec<u8>) -> Option<u8> {
    v::detect_version(&payload)
}

pub fn build_message(audio: Vec<u8>, cfg: BuildConfig) -> Result<EncodedMessage, VoiceError> {
    let encryption = cfg
        .channel_psk
        .as_ref()
        .map(|psk| v::derive_key(psk, cfg.message_id, cfg.from_node_num));
    let core_cfg = v::BuildConfig {
        message_id: cfg.message_id,
        stream_seq: cfg.stream_seq,
        codec: cfg.codec.into(),
        codec_param: cfg.codec_param,
        chunk_size: cfg.chunk_size as usize,
        parity_count: cfg.parity_count,
        last_in_stream: cfg.last_in_stream,
        encryption,
    };
    Ok(v::build_message(&audio, &core_cfg)?.into())
}

#[derive(Debug)]
pub struct NackConfig {
    pub message_id: u32,
    pub stream_seq: u8,
    pub codec: VoiceCodec,
    pub codec_param: u8,
    pub total_data: u8,
    pub parity_count: u8,
    pub missing: Vec<u8>,
    pub give_up: bool,
}

pub fn build_nack(cfg: NackConfig) -> Vec<u8> {
    v::build_nack(
        cfg.message_id,
        cfg.stream_seq,
        cfg.codec.into(),
        cfg.codec_param,
        cfg.total_data,
        cfg.parity_count,
        &cfg.missing,
        cfg.give_up,
    )
}

// -----------------------------------------------------------------------------
// Receive side: messages + events
// -----------------------------------------------------------------------------

#[derive(Debug)]
pub struct VoiceMessageOut {
    pub message_id: u32,
    pub from: String,
    pub broadcast: bool,
    pub to_node: u32,
    pub stream_seq: u8,
    pub codec: VoiceCodec,
    pub codec_param: u8,
    pub audio: Vec<u8>,
    pub timestamp_ms: i64,
    pub is_complete: bool,
    pub total_data: u8,
    pub received_data: u8,
    pub recovered_via_fec: u8,
    pub channel: u32,
    pub encrypted: bool,
}

impl From<v::VoiceMessage> for VoiceMessageOut {
    fn from(m: v::VoiceMessage) -> Self {
        let (broadcast, to_node) = match m.to {
            v::VoiceDestination::Broadcast => (true, 0),
            v::VoiceDestination::Node(n) => (false, n),
        };
        Self {
            message_id: m.message_id,
            from: m.from,
            broadcast,
            to_node,
            stream_seq: m.stream_seq,
            codec: m.codec.into(),
            codec_param: m.codec_param,
            audio: m.audio,
            timestamp_ms: m.timestamp.timestamp_millis(),
            is_complete: m.is_complete,
            total_data: m.total_data,
            received_data: m.received_data,
            recovered_via_fec: m.recovered_via_fec,
            channel: m.channel,
            encrypted: m.encrypted,
        }
    }
}

#[derive(Debug)]
pub struct NackInfo {
    pub message_id: u32,
    pub stream_seq: u8,
    pub total_data: u8,
    pub parity_count: u8,
    pub give_up: bool,
    pub missing: Vec<u8>,
}

impl From<v::NackInfo> for NackInfo {
    fn from(n: v::NackInfo) -> Self {
        Self {
            message_id: n.message_id,
            stream_seq: n.stream_seq,
            total_data: n.total_data,
            parity_count: n.parity_count,
            give_up: n.give_up,
            missing: n.missing,
        }
    }
}

#[derive(Debug)]
pub enum AssemblyEvent {
    Pending {
        message_id: u32,
        from: String,
        received_data: u8,
        total_data: u8,
        channel: u32,
    },
    Duplicate,
    Rejected {
        message: String,
    },
    Complete {
        message: VoiceMessageOut,
    },
    Nack {
        info: NackInfo,
    },
}

impl From<v::AssemblyEvent> for AssemblyEvent {
    fn from(e: v::AssemblyEvent) -> Self {
        match e {
            v::AssemblyEvent::Pending {
                message_id,
                from,
                received_data,
                total_data,
                channel,
            } => Self::Pending {
                message_id,
                from,
                received_data,
                total_data,
                channel,
            },
            v::AssemblyEvent::Duplicate => Self::Duplicate,
            v::AssemblyEvent::Rejected(err) => Self::Rejected {
                message: err.to_string(),
            },
            v::AssemblyEvent::Complete(msg) => Self::Complete {
                message: (*msg).into(),
            },
            v::AssemblyEvent::Nack(info) => Self::Nack { info: info.into() },
        }
    }
}

#[derive(Debug)]
pub struct OutboundNack {
    pub from: String,
    pub channel: u32,
    pub frame: Vec<u8>,
    pub give_up: bool,
}

impl From<v::OutboundNack> for OutboundNack {
    fn from(n: v::OutboundNack) -> Self {
        Self {
            from: n.from,
            channel: n.channel,
            frame: n.frame,
            give_up: n.give_up,
        }
    }
}

#[derive(Debug)]
pub struct TickOutput {
    pub finalized: Vec<VoiceMessageOut>,
    pub nacks: Vec<OutboundNack>,
}

impl From<v::TickOutput> for TickOutput {
    fn from(t: v::TickOutput) -> Self {
        Self {
            finalized: t.finalized.into_iter().map(Into::into).collect(),
            nacks: t.nacks.into_iter().map(Into::into).collect(),
        }
    }
}

// -----------------------------------------------------------------------------
// Assembler
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AssemblerConfig {
    pub message_timeout_ms: u64,
    pub partial_play_on_timeout: bool,
    pub channel_psk: Option<Vec<u8>>,
    pub max_nack_rounds: u8,
    pub nack_window_ms: u64,
    pub completion_memory_ms: u64,
}

impl From<AssemblerConfig> for v::AssemblerConfig {
    fn from(c: AssemblerConfig) -> Self {
        Self {
            message_timeout: Duration::from_millis(c.message_timeout_ms),
            partial_play_on_timeout: c.partial_play_on_timeout,
            channel_psk: c.channel_psk,
            max_nack_rounds: c.max_nack_rounds,
            nack_window: Duration::from_millis(c.nack_window_ms),
            completion_memory: Duration::from_millis(c.completion_memory_ms),
            // Bridge surface doesn't expose a codec allowlist yet (the
            // Android UI hasn't grown a "which codecs do we play back?"
            // setting). `None` keeps the legacy behaviour: accept any
            // known codec, defer codec-mismatch errors to playback.
            supported_codecs: None,
        }
    }
}

/// Receive-side state machine. Thin wrapper around
/// [`v::VoiceAssembler`]; the upstream type is already `Send + Sync` so
/// the lock semantics carry through transparently.
pub struct VoiceAssembler(v::VoiceAssembler);

impl VoiceAssembler {
    pub fn new(cfg: AssemblerConfig) -> Self {
        Self(v::VoiceAssembler::new(cfg.into()))
    }

    pub fn set_config(&self, cfg: AssemblerConfig) {
        self.0.set_config(cfg.into());
    }

    pub fn accept(
        &self,
        from: String,
        broadcast: bool,
        to_node: u32,
        channel: u32,
        frame: Vec<u8>,
    ) -> AssemblyEvent {
        let to = if broadcast {
            v::VoiceDestination::Broadcast
        } else {
            v::VoiceDestination::Node(to_node)
        };
        self.0.accept(&from, to, channel, &frame).into()
    }

    pub fn tick(&self) -> TickOutput {
        self.0.tick().into()
    }
}

// -----------------------------------------------------------------------------
// Settings facade
// -----------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("unknown setting key")]
    UnknownKey,
    #[error("invalid value")]
    Invalid,
    #[error("settings I/O failed")]
    Io,
}

impl From<s::SettingsError> for SettingsError {
    fn from(e: s::SettingsError) -> Self {
        match e {
            s::SettingsError::UnknownKey(_) => Self::UnknownKey,
            s::SettingsError::Invalid { .. } => Self::Invalid,
            s::SettingsError::Io(_) => Self::Io,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKey {
    LastDevice,
    VoiceMaxDurationSecs,
    VoiceReassemblyTimeoutSecs,
    VoiceCodec,
    VoiceCodec2Mode,
    VoiceAmrnbMode,
}

impl From<SettingKey> for s::SettingKey {
    fn from(k: SettingKey) -> Self {
        match k {
            SettingKey::LastDevice => Self::LastDevice,
            SettingKey::VoiceMaxDurationSecs => Self::VoiceMaxDurationSecs,
            SettingKey::VoiceReassemblyTimeoutSecs => Self::VoiceReassemblyTimeoutSecs,
            SettingKey::VoiceCodec => Self::VoiceCodec,
            SettingKey::VoiceCodec2Mode => Self::VoiceCodec2Mode,
            SettingKey::VoiceAmrnbMode => Self::VoiceAmrnbMode,
        }
    }
}

impl From<s::SettingKey> for SettingKey {
    fn from(k: s::SettingKey) -> Self {
        match k {
            s::SettingKey::LastDevice => Self::LastDevice,
            s::SettingKey::VoiceMaxDurationSecs => Self::VoiceMaxDurationSecs,
            s::SettingKey::VoiceReassemblyTimeoutSecs => Self::VoiceReassemblyTimeoutSecs,
            s::SettingKey::VoiceCodec => Self::VoiceCodec,
            s::SettingKey::VoiceCodec2Mode => Self::VoiceCodec2Mode,
            s::SettingKey::VoiceAmrnbMode => Self::VoiceAmrnbMode,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceCodecKind {
    Opus,
    Codec2,
    AmrNb,
}

impl From<VoiceCodecKind> for s::VoiceCodecKind {
    fn from(k: VoiceCodecKind) -> Self {
        match k {
            VoiceCodecKind::Opus => Self::Opus,
            VoiceCodecKind::Codec2 => Self::Codec2,
            VoiceCodecKind::AmrNb => Self::AmrNb,
        }
    }
}

impl From<s::VoiceCodecKind> for VoiceCodecKind {
    fn from(k: s::VoiceCodecKind) -> Self {
        match k {
            s::VoiceCodecKind::Opus => Self::Opus,
            s::VoiceCodecKind::Codec2 => Self::Codec2,
            s::VoiceCodecKind::AmrNb => Self::AmrNb,
        }
    }
}

#[derive(Debug, Clone)]
pub enum SettingKind {
    OptionalString,
    IntRange { min: u32, max: u32 },
    EnumVariants { variants: Vec<String> },
}

impl From<s::SettingKind> for SettingKind {
    fn from(k: s::SettingKind) -> Self {
        match k {
            s::SettingKind::OptionalString => Self::OptionalString,
            s::SettingKind::IntRange { min, max } => Self::IntRange { min, max },
            s::SettingKind::Enum { variants } => Self::EnumVariants {
                variants: variants.iter().map(|v| (*v).to_string()).collect(),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct SettingDescriptor {
    pub key: SettingKey,
    pub label: String,
    pub help: String,
    pub kind: SettingKind,
    pub value: String,
    pub default_value: String,
}

impl From<s::SettingDescriptor> for SettingDescriptor {
    fn from(d: s::SettingDescriptor) -> Self {
        Self {
            key: d.key.into(),
            label: d.label.to_string(),
            help: d.help.to_string(),
            kind: d.kind.into(),
            value: d.value,
            default_value: d.default,
        }
    }
}

pub fn setting_key_id(key: SettingKey) -> String {
    s::SettingKey::from(key).id().to_string()
}

pub fn setting_key_from_id(id: String) -> Option<SettingKey> {
    s::SettingKey::from_id(&id).map(Into::into)
}

/// Settings facade mirror. Android passes its `filesDir` as `path` so
/// the TOML store lives under the app's private data directory.
pub struct SettingsApi(std::sync::Arc<s::SettingsApi>);

impl SettingsApi {
    pub fn open_at(path: Option<String>) -> Self {
        Self(s::SettingsApi::open_at(path.map(std::path::PathBuf::from)))
    }

    pub fn get_str(&self, key: SettingKey) -> String {
        self.0.get_str(key.into())
    }

    pub fn set_str(&self, key: SettingKey, value: String) -> Result<(), SettingsError> {
        self.0.set_str(key.into(), &value).map_err(Into::into)
    }

    pub fn reset(&self, key: SettingKey) -> Result<(), SettingsError> {
        self.0.reset(key.into()).map_err(Into::into)
    }

    pub fn reset_all(&self) -> Result<(), SettingsError> {
        self.0.reset_all().map_err(Into::into)
    }

    pub fn list(&self) -> Vec<SettingDescriptor> {
        self.0.list().into_iter().map(Into::into).collect()
    }

    pub fn last_device(&self) -> Option<String> {
        self.0.last_device()
    }

    pub fn set_last_device(&self, value: Option<String>) -> Result<(), SettingsError> {
        self.0.set_last_device(value).map_err(Into::into)
    }

    pub fn voice_max_secs(&self) -> u32 {
        self.0.voice_max_secs()
    }

    pub fn set_voice_max_secs(&self, secs: u32) -> Result<(), SettingsError> {
        self.0.set_voice_max_secs(secs).map_err(Into::into)
    }

    pub fn reassembly_timeout_secs(&self) -> u32 {
        self.0.reassembly_timeout_secs()
    }

    pub fn set_reassembly_timeout_secs(&self, secs: u32) -> Result<(), SettingsError> {
        self.0.set_reassembly_timeout_secs(secs).map_err(Into::into)
    }

    pub fn voice_codec(&self) -> VoiceCodecKind {
        self.0.voice_codec().into()
    }

    pub fn set_voice_codec(&self, kind: VoiceCodecKind) -> Result<(), SettingsError> {
        self.0.set_voice_codec(kind.into()).map_err(Into::into)
    }

    pub fn voice_codec2_mode(&self) -> u8 {
        self.0.voice_codec2_mode()
    }

    pub fn set_voice_codec2_mode(&self, mode: u8) -> Result<(), SettingsError> {
        self.0.set_voice_codec2_mode(mode).map_err(Into::into)
    }

    pub fn voice_amrnb_mode(&self) -> u8 {
        self.0.voice_amrnb_mode()
    }

    pub fn set_voice_amrnb_mode(&self, mode: u8) -> Result<(), SettingsError> {
        self.0.set_voice_amrnb_mode(mode).map_err(Into::into)
    }
}

uniffi::include_scaffolding!("voicetastic");

mod mesh_service;
mod runtime;
mod smoke;

pub use mesh_service::{
    IncomingDataMsg, IncomingTextMsg, MeshConfigListener, MeshConnectionState, MeshDataListener,
    MeshQueueListener, MeshService, MeshServiceError, MeshStateListener, MeshTextListener,
    MeshTransport, MeshTransportSink, QueueStatusEvent, node_id_to_num, node_num_to_id,
};
