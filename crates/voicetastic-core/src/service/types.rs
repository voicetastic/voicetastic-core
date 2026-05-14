//! Public value types and small helpers used by [`super::MeshService`].

use crate::proto::{NodeInfo, User};

/// Coarse connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Configuring,
    Ready,
}

/// One inbound text message routed off the mesh.
#[derive(Debug, Clone)]
pub struct IncomingText {
    pub from: u32,
    pub from_id: String,
    pub to: u32,
    pub channel: u32,
    pub text: String,
    pub rx_time: u32,
    pub rx_snr: f32,
    pub rx_rssi: i32,
}

/// One inbound application data packet (used for voice + private-app).
#[derive(Debug, Clone)]
pub struct IncomingData {
    pub from: u32,
    pub to: u32,
    pub channel: u32,
    pub portnum: i32,
    pub payload: Vec<u8>,
    pub rx_time: u32,
}

/// Mirror of the firmware's `QueueStatus` event. Fired every time the
/// device's outbound packet queue accepts or drains an entry; lets
/// callers track when a specific outgoing packet has actually left the
/// firmware (≈ "the radio has transmitted it on air").
#[derive(Debug, Clone, Copy)]
pub struct QueueStatusEvent {
    /// `ErrorCode` of the last queue operation; 0 == success.
    pub res: i32,
    /// Free slots remaining in the firmware queue.
    pub free: u32,
    /// Maximum number of slots.
    pub maxlen: u32,
    /// Mesh packet id this status applies to, or 0 if not associated
    /// with a specific packet.
    pub mesh_packet_id: u32,
}

/// Long name accessor for callers that don't want to import `proto::User`.
pub fn node_long_name(node: &NodeInfo) -> Option<&str> {
    node.user.as_ref().map(|u: &User| u.long_name.as_str())
}

pub(super) fn rand_u32() -> Result<u32, crate::Error> {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).map_err(|e| crate::Error::Other(e.to_string()))?;
    Ok(u32::from_ne_bytes(buf))
}
