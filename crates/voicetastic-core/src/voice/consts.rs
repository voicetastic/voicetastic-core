//! Protocol-wide constants. See `VOICE_PROTOCOL.md` Appendix A.

use std::time::Duration;

/// On-wire version byte.
pub const PROTOCOL_VERSION: u8 = 0x01;
/// Fixed header length preceding every frame.
pub const HEADER_SIZE: usize = 12;
/// Maximum total frame size (header + body) — Meshtastic LoRa MTU.
pub const MAX_PACKET_SIZE: usize = 231;
/// Maximum body bytes per frame (`MAX_PACKET_SIZE - HEADER_SIZE`).
pub const MAX_BODY_SIZE: usize = MAX_PACKET_SIZE - HEADER_SIZE;
/// Minimum allowed `chunk_size`.
pub const MIN_CHUNK_SIZE: usize = 16;
/// Maximum data chunks per message (`total_data` field is `u8`).
pub const MAX_CHUNKS_PER_MESSAGE: usize = 255;
/// Hard receive-side cap on the un-FEC payload of a single message
/// (`MAX_CHUNKS_PER_MESSAGE * MAX_BODY_SIZE`). Frames pushing the assembler
/// past this are rejected.
pub const MAX_MESSAGE_BYTES: usize = MAX_CHUNKS_PER_MESSAGE * MAX_BODY_SIZE;
/// Maximum parity chunks per message (Reed-Solomon coder limit).
pub const MAX_PARITY_PER_MESSAGE: usize = 128;
/// Global cap on concurrent in-progress reassemblies.
pub const MAX_IN_PROGRESS_GLOBAL: usize = 64;
/// Per-sender cap on concurrent in-progress reassemblies.
pub const MAX_IN_PROGRESS_PER_SENDER: usize = 4;
/// Recently-completed message blacklist TTL.
pub const BLACKLIST_TTL: Duration = Duration::from_secs(60);
/// Recently-completed blacklist max entries.
pub const BLACKLIST_MAX: usize = 100;
/// Maximum NACK rounds per message before the receiver gives up.
pub const NACK_MAX_ROUNDS: u8 = 3;
/// Quiet-period after the last seen chunk before issuing a NACK.
pub const NACK_WINDOW_MS: u64 = 1500;
/// AES-GCM nonce length (96 bits per RFC 5288).
pub const GCM_NONCE_LEN: usize = 12;
/// AES-GCM authentication tag length (128 bits).
pub const GCM_TAG_LEN: usize = 16;
