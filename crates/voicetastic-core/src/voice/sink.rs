//! Voice frame transmission interface.
//!
//! `VoiceFrameSink` is the minimal interface `VoiceSender` needs to do its job:
//! enqueue frames and listen for inbound NACK packets. It exists so the
//! sender doesn't have to name a concrete service type.

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::Result;
use crate::node::NodeId;
use crate::voice::consts::MAX_BODY_SIZE;
use crate::voice::types::VoiceData;

/// Minimal interface for voice frame transmission.
///
/// `VoiceSender` uses this to send bursts and retransmits, and to subscribe to
/// inbound NACK packets.
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

    /// Maximum voice-frame body size (excluding the 16-byte chunk
    /// header) that this sink can carry intact in a single outbound
    /// frame. The voice sender uses this to size [`SendRequest::chunk_size`]
    /// when the caller doesn't specify one, and to clamp explicit
    /// requests down to a transport-safe value.
    ///
    /// Returns [`MAX_BODY_SIZE`] by default for sinks with no fixed
    /// per-frame cap (loopback, USB serial). BLE-backed sinks return
    /// `transport_mtu − 3 − HEADER_SIZE − ToRadio_overhead`.
    fn max_voice_body_size(&self) -> usize {
        MAX_BODY_SIZE
    }
}
