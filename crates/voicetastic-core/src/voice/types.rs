//! Voice addressing types. The wire-shape enums (`PacketType`, `VoiceCodec`,
//! `ModemPreset`) live in the no_std `voicetastic-proto` crate and are
//! re-exported here so existing `crate::voice::types::*` paths keep working.
//! The `NodeId`-carrying addressing types stay in core (driver layer).

pub use voicetastic_proto::types::{ModemPreset, PacketType, VoiceCodec};

use serde::{Deserialize, Serialize};

use crate::node::NodeId;

/// Destination of a voice message: a specific node or the channel broadcast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VoiceDestination {
    Node(NodeId),
    Broadcast,
}

/// Inbound voice frame after protocol filtering (port + version check),
/// ready to hand to the assembler.
#[derive(Debug, Clone)]
pub struct VoiceData {
    pub from: NodeId,
    pub to: VoiceDestination,
    pub channel: u32,
    pub payload: Vec<u8>,
}
