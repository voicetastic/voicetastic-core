//! Delivery-ack tracking for outbound DMs.
//!
//! Meshtastic firmware emits an inbound `meshtastic.Routing` packet on
//! [`super::ports::ROUTING_APP`] for every outbound packet that had
//! `want_ack = true`. The inner `Data.request_id` field carries the
//! original packet's id; the `Routing.variant.ErrorReason` field
//! carries `NONE` for delivered or a typed error for failures.
//!
//! Callers register interest via the [`MeshtasticService::send_text_tracked`] /
//! [`send_data_tracked`](crate::MeshtasticService::send_data_tracked)
//! helpers, which return an [`AckHandle`] alongside the packet id. The
//! handle resolves once the firmware reports back (or the caller-supplied
//! deadline elapses, via [`AckHandle::wait`]).

use std::time::Duration;

use tokio::sync::oneshot;

use crate::proto::routing;

/// Resolution of an outbound DM's delivery ack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckResult {
    /// Firmware reported `Routing::Error::None` — delivered (or at
    /// least accepted by the next hop on a flood-routed mesh).
    Delivered,
    /// Firmware reported a typed delivery failure. See `routing::Error`
    /// for the variants — common ones are `NoRoute`, `Timeout`,
    /// `MaxRetransmit`, `NotAuthorized`.
    Failed(routing::Error),
    /// No `Routing` packet arrived before the caller-supplied deadline.
    /// The original packet may still be in flight; the mesh just hasn't
    /// reported back yet.
    TimedOut,
    /// The service was dropped (e.g. disconnect) before the ack arrived.
    Cancelled,
}

impl AckResult {
    /// True if the packet was actually delivered. `false` for any failure
    /// or timeout — useful when mapping to a CLI exit code.
    pub fn is_delivered(self) -> bool {
        matches!(self, AckResult::Delivered)
    }
}

/// Receive end of a single packet's delivery ack. Cheap to drop if the
/// caller doesn't care about the result — the matching service-side
/// `Sender` will close cleanly.
#[derive(Debug)]
pub struct AckHandle {
    packet_id: u32,
    rx: oneshot::Receiver<AckResult>,
}

impl AckHandle {
    pub(crate) fn new(packet_id: u32, rx: oneshot::Receiver<AckResult>) -> Self {
        Self { packet_id, rx }
    }

    /// The packet id this handle tracks.
    pub fn packet_id(&self) -> u32 {
        self.packet_id
    }

    /// Wait up to `timeout` for the firmware to report delivery status.
    /// Returns [`AckResult::TimedOut`] if the deadline elapses without
    /// a `Routing` packet, [`AckResult::Cancelled`] if the service was
    /// dropped first.
    pub async fn wait(self, timeout: Duration) -> AckResult {
        match tokio::time::timeout(timeout, self.rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => AckResult::Cancelled,
            Err(_) => AckResult::TimedOut,
        }
    }
}
