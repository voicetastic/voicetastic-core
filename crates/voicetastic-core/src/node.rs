//! Protocol-agnostic node identity and summary.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Opaque node identifier. Protocol-agnostic wrapper so Meshtastic (u32 node
/// numbers) and Meshcore (different addressing) can both implement `RadioService`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub u32);

impl NodeId {
    /// Create a node ID from a u32. Meshtastic-specific for now; meshcore
    /// will have its own constructor if needed.
    pub fn from_u32(n: u32) -> Self {
        Self(n)
    }

    /// Convert to u32. Only safe for Meshtastic; meshcore impls will need
    /// their own extraction methods.
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

impl fmt::Display for NodeId {
    /// Format as Meshtastic style: `!aabbccdd` (8 hex digits with `!` prefix).
    /// Meshcore impls can override if needed, but this is convenient.
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

/// Minimal node information exposed to frontends (protocol-agnostic).
/// Meshtastic provides these via NodeInfo; Meshcore provides equivalent data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSummary {
    pub id: NodeId,
    pub short_name: Option<String>,
    pub long_name: Option<String>,
}
