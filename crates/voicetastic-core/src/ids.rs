//! Node-id helpers.
//!
//! Meshtastic uses a `u32` node number internally; the textual form used in the
//! UI is `"!" + 8 lowercase hex digits` (e.g. `!a1b2c3d4`). Mirrors
//! `MeshtasticBle.nodeNumToId` / `nodeIdToNum` from the Android app.

use crate::error::{Error, Result};

/// Format a node number as `!aabbccdd`.
pub fn node_num_to_id(num: u32) -> String {
    format!("!{:08x}", num)
}

/// Parse a `!aabbccdd` node id into a node number.
pub fn node_id_to_num(id: &str) -> Result<u32> {
    let trimmed = id.strip_prefix('!').ok_or_else(|| Error::InvalidNodeId(id.into()))?;
    if trimmed.len() != 8 {
        return Err(Error::InvalidNodeId(id.into()));
    }
    u32::from_str_radix(trimmed, 16).map_err(|_| Error::InvalidNodeId(id.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for n in [0u32, 1, 0xa1b2_c3d4, 0xffff_ffff] {
            let s = node_num_to_id(n);
            assert_eq!(node_id_to_num(&s).unwrap(), n);
        }
    }

    #[test]
    fn format_is_lowercase_hex_8() {
        assert_eq!(node_num_to_id(0xA1B2_C3D4), "!a1b2c3d4");
        assert_eq!(node_num_to_id(0), "!00000000");
    }

    #[test]
    fn rejects_bad_inputs() {
        assert!(node_id_to_num("a1b2c3d4").is_err());
        assert!(node_id_to_num("!a1b2c3d").is_err());
        assert!(node_id_to_num("!a1b2c3d4e").is_err());
        assert!(node_id_to_num("!ZZZZZZZZ").is_err());
    }
}
