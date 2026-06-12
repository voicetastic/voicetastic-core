//! Errors raised by the voice protocol layer.

use thiserror::Error;

use crate::consts::MIN_CHUNK_SIZE;
use crate::types::VoiceCodec;

/// Errors raised by the voice protocol layer.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VoiceError {
    #[error("packet too short ({len} bytes, need ≥ {needed})")]
    TooShort { len: usize, needed: usize },
    #[error("packet too large ({len} bytes, max {max})")]
    TooLarge { len: usize, max: usize },
    #[error("unsupported protocol version byte: 0x{0:02x}")]
    BadVersion(u8),
    #[error("reserved type_flags bit set: 0x{0:02x}")]
    ReservedFlagSet(u8),
    #[error("reserved packet_type")]
    ReservedPacketType,
    #[error("message_id must be non-zero")]
    ZeroMessageId,
    #[error("invalid totalData: {0}")]
    BadTotal(u8),
    #[error("parity_count {0} exceeds MAX_PARITY_PER_MESSAGE")]
    TooMuchParity(u8),
    #[error("data + parity shards ({data} + {parity}) exceed Reed-Solomon limit of 256")]
    TooManyShards { data: u8, parity: u8 },
    #[error("chunk_index {idx} out of range for total {total}")]
    BadIndex { idx: u8, total: u8 },
    #[error("audio too large: {bytes} B exceeds maximum {max} B per message")]
    AudioTooLarge { bytes: usize, max: usize },
    #[error("chunk_size {0} below minimum {MIN_CHUNK_SIZE}")]
    ChunkTooSmall(usize),
    #[error("chunk_size {got} exceeds maximum body size {max}")]
    ChunkTooLarge { got: usize, max: usize },
    #[error("data body length {got} != established chunk_size {expected}")]
    BodyLenMismatch { got: usize, expected: usize },
    #[error("NACK frame body too short")]
    NackTooShort,
    #[error("Reed-Solomon error: {0}")]
    Fec(String),
    #[error("codec mismatch within message: {first:?} vs {got:?}")]
    CodecMismatch { first: VoiceCodec, got: VoiceCodec },
    #[error("total_data mismatch within message: {first} vs {got}")]
    TotalMismatch { first: u8, got: u8 },
    #[error("stream_seq mismatch within message: {first} vs {got}")]
    StreamSeqMismatch { first: u8, got: u8 },
    #[error("parity_count decreased within message: first={first}, got={got}")]
    ParityCountDecrease { first: u8, got: u8 },
    #[error("NACK frame chunk_index must be 0, got {0}")]
    BadNackIndex(u8),
    #[error("unknown codec byte: 0x{0:02x}")]
    UnknownCodec(u8),
    #[error("codec {0:?} is not supported by this receiver")]
    UnsupportedCodec(VoiceCodec),
    #[error("(from, message_id) is on the recently-completed blacklist")]
    Blacklisted,
    #[error("per-sender in-flight cap reached for {0}")]
    PerSenderCap(String),
    #[error("header MAC verification failed")]
    BadMac,
    #[error("OS RNG unavailable: {0}")]
    Rng(String),
}

/// Convenience alias for voice protocol results.
pub type Result<T> = core::result::Result<T, VoiceError>;

use alloc::string::String;
