//! Reassembled voice message + assembler-event sum type.

use super::error::VoiceError;
use super::nack::NackInfo;
use super::types::{VoiceCodec, VoiceDestination};

/// Reassembled voice message emitted by the assembler.
#[derive(Debug, Clone)]
pub struct VoiceMessage {
    pub message_id: u32,
    pub from: String,
    pub to: VoiceDestination,
    pub stream_seq: u8,
    pub codec: VoiceCodec,
    pub codec_param: u8,
    /// Codec frame bytes (no container header). Caller wraps for playback.
    pub audio: Vec<u8>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub is_complete: bool,
    pub total_data: u8,
    pub received_data: u8,
    pub recovered_via_fec: u8,
    pub channel: u32,
}

/// Outcome of feeding a frame to the assembler's `accept`.
#[derive(Debug)]
pub enum AssemblyEvent {
    /// Frame accepted, message still in progress. Carries enough info
    /// for the caller to update a live "received X/Y chunks" UI.
    Pending {
        message_id: u32,
        from: String,
        received_data: u8,
        total_data: u8,
        channel: u32,
    },
    /// Frame was a duplicate (same chunk_index already stored). Dropped.
    Duplicate,
    /// Frame rejected (blacklist, decrypt-fail, structural error, …).
    Rejected(VoiceError),
    /// Message complete. The caller may also want to stop sending NACKs.
    Complete(Box<VoiceMessage>),
    /// NACK frame parsed; route to the sender's send-side state.
    Nack(NackInfo),
}
