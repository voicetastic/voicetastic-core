//! Voice frame transmission interface, decoupled from any concrete radio service.
//!
//! `VoiceFrameSink` is the minimal interface `VoiceSender` needs to do its job:
//! enqueue frames and listen for NACK packets. This lets `VoiceSender` work with
//! any radio service (Meshtastic, Meshcore, etc.) without depending on its concrete type.

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::Result;
use crate::node::NodeId;
use crate::radio_service::VoiceData;

/// Minimal interface for voice frame transmission.
///
/// `VoiceSender` uses this to send bursts and retransmits, and to subscribe to
/// inbound NACK packets. Both `MeshtasticService` and future protocol impls will
/// implement this.
#[async_trait]
pub trait VoiceFrameSink: Send + Sync {
    /// Enqueue a voice frame for transmission. The service handles protocol-specific
    /// wrapping (Meshtastic: `PRIVATE_APP` portnum, FEC, pacing, etc.).
    /// Returns the packet ID assigned by the service (for tracking and NACK mapping).
    async fn enqueue_voice_frame_with_id(
        &self,
        frame: Vec<u8>,
        channel: u32,
        to: Option<NodeId>,
        want_ack: bool,
        pacing: Duration,
    ) -> Result<u32>;

    /// Subscribe to inbound voice data (used by NACK listener to find packets
    /// targeting our outgoing messages). Pre-filtered; only voice frames arrive.
    fn subscribe_voice_data(&self) -> broadcast::Receiver<VoiceData>;
}
