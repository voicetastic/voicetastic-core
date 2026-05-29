//! Node identifier wrapper used across the voice and Meshtastic layers.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Opaque node identifier. Meshtastic addresses nodes by `u32`; this wrapper
/// keeps the voice protocol from having to name `u32` everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub u32);

impl NodeId {
    pub fn from_u32(n: u32) -> Self {
        Self(n)
    }

    pub fn as_u32(self) -> u32 {
        self.0
    }
}

impl fmt::Display for NodeId {
    /// Format as Meshtastic style: `!aabbccdd` (8 hex digits with `!` prefix).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "!{:08x}", self.0)
    }
}

impl std::str::FromStr for NodeId {
    type Err = crate::Error;

    /// Parse from Meshtastic format: `!aabbccdd` or just `aabbccdd`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex_part = s.strip_prefix('!').unwrap_or(s);
        u32::from_str_radix(hex_part, 16)
            .map(NodeId)
            .map_err(|_| crate::Error::InvalidNodeId(s.to_string()))
    }
}
