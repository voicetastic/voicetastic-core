//! Meshtastic application port numbers used by Voicetastic.
//!
//! Direct port of the Kotlin `Portnums.kt` constants.

/// Plain UTF-8 text chat.
pub const TEXT_MESSAGE_APP: u32 = 1;
/// Position broadcast (read-only).
pub const POSITION_APP: u32 = 3;
/// Node info beacons (read-only).
pub const NODEINFO_APP: u32 = 4;
/// Config / channel / owner writes & device actions.
pub const ADMIN_APP: u32 = 6;
/// Voice chunks. See [`crate::voice`].
pub const PRIVATE_APP: u32 = 256;

/// Meshtastic broadcast destination.
pub const BROADCAST_ADDR: u32 = 0xFFFF_FFFF;

/// Maximum accepted UTF-8 text payload size (bytes).
///
/// Meshtastic firmware caps text messages around 237 bytes; we accept a bit
/// more to tolerate future bumps but reject anything obviously oversized to
/// bound memory use. Used by both the inbound decoder and the outbound
/// `send_text` guard.
pub const MAX_TEXT_BYTES: usize = 1024;
