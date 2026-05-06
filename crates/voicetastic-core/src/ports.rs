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
