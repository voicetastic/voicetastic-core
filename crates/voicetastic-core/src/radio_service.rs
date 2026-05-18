//! Protocol-agnostic radio service abstraction.
//!
//! `RadioService` is the façade through which GUI/CLI/Android interact with a mesh
//! radio. Meshtastic and Meshcore each implement this trait; frontends use only these
//! methods and never touch protocol-specific types (proto::*, port constants, etc.).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc, watch};

use crate::Result;
use crate::node::NodeId;
use crate::transport::Transport;
use crate::voice::VoiceDestination;

/// Connection state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Configuring,
    Ready,
}

/// Inbound text message from a remote node.
#[derive(Debug, Clone)]
pub struct IncomingText {
    pub from: NodeId,
    pub to: NodeId,
    pub channel: u32,
    pub text: String,
    pub rx_time: i64,
}

/// Pre-filtered inbound voice data, ready for reassembly.
/// Protocol filtering (PRIVATE_APP, version check) already done by the service.
#[derive(Debug, Clone)]
pub struct VoiceData {
    pub from: NodeId,
    pub to: VoiceDestination,
    pub channel: u32,
    pub payload: Vec<u8>,
}

/// Firmware queue backpressure event (modem-specific but fields are generic).
#[derive(Debug, Clone)]
pub struct QueueEvent {
    pub free: u32,
    pub mesh_packet_id: Option<u32>,
}

/// Protocol-agnostic radio service interface.
///
/// Both `MeshtasticService` and future `MeshcoreService` implement this.
/// Frontends (GUI, CLI, Android bridge) depend only on this trait and never
/// on concrete protocol types.
#[async_trait]
pub trait RadioService: Send + Sync {
    /// Connect to a radio via the provided transport. `settle_delay` is the
    /// post-connection wait before the first handshake attempt (modem-specific).
    async fn connect_with_transport(
        &self,
        transport: Arc<dyn Transport>,
        inbound: mpsc::Receiver<Vec<u8>>,
        settle_delay: Duration,
    ) -> Result<()>;

    /// Disconnect gracefully.
    async fn disconnect(&self) -> Result<()>;

    /// The local node's ID, or `None` if not yet connected.
    fn my_node_id(&self) -> Option<NodeId>;

    /// Watch the connection state machine.
    fn watch_state(&self) -> watch::Receiver<ConnectionState>;

    /// Watch the remote node roster as a HashMap. Updated asynchronously as the
    /// radio emits `NodeInfo` or equivalent.
    fn watch_nodes(&self) -> watch::Receiver<HashMap<NodeId, crate::node::NodeSummary>>;

    /// Watch the active modem preset (LoRa-specific but generic concept).
    /// Emits `None` when the preset is unknown; frontends fall back to
    /// `ModemPreset::fallback_pacing()`. Hides protocol-specific config types.
    fn watch_modem_preset(&self) -> watch::Receiver<Option<crate::voice::types::ModemPreset>>;

    /// Send a text message.
    async fn send_text(&self, text: &str, channel: u32, to: Option<NodeId>) -> Result<()>;

    /// Send a voice NACK frame back to `to_node`. The service handles protocol-specific
    /// wrapping (Meshtastic: `PRIVATE_APP` portnum, etc.).
    async fn send_voice_nack(&self, frame: Vec<u8>, channel: u32, to_node: NodeId) -> Result<()>;

    /// Subscribe to inbound text messages.
    fn subscribe_text(&self) -> broadcast::Receiver<IncomingText>;

    /// Subscribe to inbound voice data. Already filtered by protocol (port, version).
    /// Callers never touch `PRIVATE_APP`, `detect_version`, or other protocol plumbing.
    fn subscribe_voice_data(&self) -> broadcast::Receiver<VoiceData>;

    /// Subscribe to firmware queue backpressure events.
    fn subscribe_queue_event(&self) -> broadcast::Receiver<QueueEvent>;
}
