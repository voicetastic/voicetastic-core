//! Voice frame transmission interface.
//!
//! `VoiceFrameSink` is the seam between a voice driver and the radio it
//! ships frames to. The shipping crate's native [`crate::voice::sender::VoiceSender`]
//! talks to [`crate::MeshtasticService`] directly (tighter integration
//! with the inherent NACK / queue-status plumbing); this trait is the
//! intended attachment point for non-tokio drivers — primarily the
//! `wasm32` browser client, which paces voice frames through the
//! sans-IO [`crate::voice::tx_state::VoiceTx`] state machine instead of
//! the native worker.
//!
//! [`MeshtasticService`](crate::MeshtasticService) implements this trait
//! so the same service can back either driver as the browser client
//! lands.

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::Result;
use crate::node::NodeId;
use crate::voice::consts::MAX_BODY_SIZE;
use crate::voice::types::VoiceData;

/// Minimal interface for voice frame transmission.
///
/// Used by non-tokio drivers (browser/wasm) to enqueue bursts +
/// retransmits and to subscribe to inbound NACK packets. The native
/// [`crate::voice::sender::VoiceSender`] calls into
/// [`crate::MeshtasticService`] directly and does not consume this trait.
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
