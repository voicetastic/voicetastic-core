// SPDX-License-Identifier: MIT
//
//! UniFFI surface for `voicetastic_core::service::MeshService`.
//!
//! ## Threading model
//!
//! Kotlin sees a synchronous API: `MeshService::connect` returns once the
//! transport has been registered; per-call methods (`send_text`, …) block
//! until the underlying tokio future resolves. All blocking is done on the
//! shared `runtime()`; we never block a JVM thread on I/O for longer than
//! one `write_to_radio` round-trip.
//!
//! ## Transport direction inversion
//!
//! Core's [`voicetastic_core::Transport`] models only the outbound half;
//! inbound frames arrive via a separate `mpsc::Receiver<Vec<u8>>` passed at
//! connect time. Across the FFI we invert that: Kotlin implements
//! [`MeshTransport`] (a foreign callback trait) for the *outbound* half,
//! and Rust hands back a [`MeshTransportSink`] handle that Kotlin pumps
//! whenever a BLE notify / USB read produces a frame. Internally the sink
//! wraps the `mpsc::Sender` end of the channel `MeshService` reads from.
//!
//! ## Why opaque protobuf bytes
//!
//! Per-section `Config` / `Channel` / `User` / `MyNodeInfo` / `NodeInfo`
//! are delivered to Kotlin as already-encoded `bytes`. This avoids
//! re-modelling 30+ proto fields in UDL — both sides have a proto
//! compiler, and the wire format is the contract anyway.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use prost::Message as _;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

use voicetastic_core::proto::{AdminMessage, Channel, Config, MyNodeInfo, NodeInfo, User, config};
use voicetastic_core::service::{
    ConnectionState as CoreConnState, IncomingData as CoreIncomingData,
    IncomingText as CoreIncomingText, MeshService as CoreMeshService,
    QueueStatusEvent as CoreQueueStatusEvent,
};
use voicetastic_core::transport::Transport;
use voicetastic_core::voice as v;

use crate::BuildConfig;
use crate::runtime::runtime;

// -----------------------------------------------------------------------------
// Errors
// -----------------------------------------------------------------------------

/// Errors raised by [`MeshService`] operations.
///
/// Mirrors the variants of `voicetastic_core::Error` that can plausibly
/// reach an Android caller; the BLE / serial / btleplug-specific variants
/// are coalesced into [`MeshServiceError::Transport`] because the bridge
/// is built without the desktop transports.
#[derive(Debug, thiserror::Error)]
pub enum MeshServiceError {
    #[error("not connected to a Meshtastic node")]
    NotConnected,
    #[error("local node info not yet received (my_node_num is 0)")]
    NoLocalNode,
    #[error("transport error: {error_message}")]
    Transport { error_message: String },
    #[error("protocol error: {error_message}")]
    Protocol { error_message: String },
    #[error("voice protocol error: {error_message}")]
    Voice { error_message: String },
    #[error("invalid argument: {error_message}")]
    InvalidArgument { error_message: String },
    #[error("{error_message}")]
    Other { error_message: String },
}

impl From<voicetastic_core::Error> for MeshServiceError {
    fn from(e: voicetastic_core::Error) -> Self {
        use voicetastic_core::Error as E;
        match e {
            E::NotConnected => Self::NotConnected,
            E::NoLocalNode => Self::NoLocalNode,
            E::ProtoDecode(err) => Self::Protocol {
                error_message: err.to_string(),
            },
            E::ProtoEncode(err) => Self::Protocol {
                error_message: err.to_string(),
            },
            E::Voice(err) => Self::Voice {
                error_message: err.to_string(),
            },
            E::Io(err) => Self::Transport {
                error_message: err.to_string(),
            },
            E::WriteTimeout => Self::Transport {
                error_message: "write timeout".into(),
            },
            E::MissingCharacteristic(name) => Self::Transport {
                error_message: format!("missing GATT characteristic: {name}"),
            },
            E::InvalidNodeId(s) => Self::InvalidArgument {
                error_message: format!("invalid node id: {s}"),
            },
            E::Other(msg) => Self::Other { error_message: msg },
            // BLE variant only exists with the ble-btleplug feature, which
            // the Android bridge intentionally disables; the catch-all keeps
            // the match exhaustive should that change.
            #[allow(unreachable_patterns)]
            _ => Self::Other {
                error_message: e.to_string(),
            },
        }
    }
}

impl From<v::VoiceError> for MeshServiceError {
    fn from(e: v::VoiceError) -> Self {
        Self::Voice {
            error_message: e.to_string(),
        }
    }
}

// -----------------------------------------------------------------------------
// Connection state (UDL-friendly mirror of core::ConnectionState)
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Configuring,
    Ready,
}

impl From<CoreConnState> for MeshConnectionState {
    fn from(s: CoreConnState) -> Self {
        match s {
            CoreConnState::Disconnected => Self::Disconnected,
            CoreConnState::Connecting => Self::Connecting,
            CoreConnState::Connected => Self::Connected,
            CoreConnState::Configuring => Self::Configuring,
            CoreConnState::Ready => Self::Ready,
        }
    }
}

// -----------------------------------------------------------------------------
// Incoming event dictionaries
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct IncomingTextMsg {
    pub from: u32,
    pub from_id: String,
    pub to: u32,
    pub channel: u32,
    pub text: String,
    pub rx_time: u32,
    pub rx_snr: f32,
    pub rx_rssi: i32,
}

impl From<CoreIncomingText> for IncomingTextMsg {
    fn from(t: CoreIncomingText) -> Self {
        Self {
            from: t.from,
            from_id: t.from_id,
            to: t.to,
            channel: t.channel,
            text: t.text,
            rx_time: t.rx_time,
            rx_snr: t.rx_snr,
            rx_rssi: t.rx_rssi,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IncomingDataMsg {
    pub from: u32,
    pub to: u32,
    pub channel: u32,
    pub portnum: i32,
    pub payload: Vec<u8>,
    pub rx_time: u32,
}

impl From<CoreIncomingData> for IncomingDataMsg {
    fn from(d: CoreIncomingData) -> Self {
        Self {
            from: d.from,
            to: d.to,
            channel: d.channel,
            portnum: d.portnum,
            payload: d.payload,
            rx_time: d.rx_time,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct QueueStatusEvent {
    pub res: i32,
    pub free: u32,
    pub maxlen: u32,
    pub mesh_packet_id: u32,
}

impl From<CoreQueueStatusEvent> for QueueStatusEvent {
    fn from(q: CoreQueueStatusEvent) -> Self {
        Self {
            res: q.res,
            free: q.free,
            maxlen: q.maxlen,
            mesh_packet_id: q.mesh_packet_id,
        }
    }
}

// -----------------------------------------------------------------------------
// Foreign callback interfaces (Kotlin implements; Rust calls)
// -----------------------------------------------------------------------------

/// Outbound half of the radio transport. Kotlin implements this for BLE
/// (GATT write) and USB-serial. Rust calls the methods from inside the
/// bridge runtime; implementers must be tolerant of being called from any
/// thread.
///
/// Methods are synchronous from Rust's view: the bridge wraps every call
/// with [`tokio::task::spawn_blocking`] so the foreign code may block on
/// platform I/O without stalling the bridge runtime.
pub trait MeshTransport: Send + Sync {
    /// Send one already-encoded `ToRadio` protobuf message.
    fn write_to_radio(&self, bytes: Vec<u8>);
    /// Close the underlying transport. Idempotent.
    fn shutdown(&self);
}

pub trait MeshStateListener: Send + Sync {
    fn on_state(&self, state: MeshConnectionState);
}
pub trait MeshTextListener: Send + Sync {
    fn on_text(&self, message: IncomingTextMsg);
}
pub trait MeshDataListener: Send + Sync {
    fn on_data(&self, message: IncomingDataMsg);
}
pub trait MeshQueueListener: Send + Sync {
    fn on_queue_status(&self, event: QueueStatusEvent);
}

/// Per-section config listener. Each call receives a fully-encoded
/// protobuf payload that Kotlin parses with its geeksville-mesh codegen.
/// The bridge intentionally does NOT re-export the proto schema across
/// UniFFI — see the module docstring.
pub trait MeshConfigListener: Send + Sync {
    /// `MyNodeInfo` proto bytes.
    fn on_my_info(&self, encoded: Vec<u8>);
    /// `NodeInfo` proto bytes (one node).
    fn on_node_info(&self, encoded: Vec<u8>);
    /// `Config` proto bytes wrapping one of the per-section variants.
    fn on_config(&self, encoded: Vec<u8>);
    /// `Channel` proto bytes (single index slot).
    fn on_channel(&self, encoded: Vec<u8>);
    /// `User` proto bytes for the local node owner.
    fn on_owner(&self, encoded: Vec<u8>);
    /// `DeviceMetadata` proto bytes.
    fn on_metadata(&self, encoded: Vec<u8>);
    /// Config burst terminator: a non-zero nonce echoed back by the
    /// firmware once it has finished pushing the burst.
    fn on_config_complete(&self, nonce: u32);
}

// -----------------------------------------------------------------------------
// MeshTransportSink — Rust object Kotlin pushes inbound frames into.
// -----------------------------------------------------------------------------

/// Handle returned by [`MeshService::connect`]. Kotlin's BLE / serial
/// callback path calls [`MeshTransportSink::push_inbound`] for every
/// decoded `FromRadio` frame; calling [`MeshTransportSink::shutdown`]
/// signals EOF (e.g. on BLE disconnect) and moves the service to
/// `Disconnected`.
pub struct MeshTransportSink {
    /// `None` after `shutdown()`; `Some` while inbound is live.
    sender: StdMutex<Option<mpsc::Sender<Vec<u8>>>>,
}

impl MeshTransportSink {
    pub fn push_inbound(&self, frame: Vec<u8>) {
        // We deliberately drop frames silently if the channel is full or
        // closed: blocking the JVM caller would risk an ANR, and the only
        // recoverable cause of a full queue is the inbound task being
        // permanently stalled (in which case we already lost the session).
        if let Some(tx) = self.sender.lock().expect("sink mutex").as_ref()
            && let Err(e) = tx.try_send(frame)
        {
            warn!(?e, "inbound sink full or closed; dropping frame");
        }
    }

    pub fn shutdown(&self) {
        self.sender.lock().expect("sink mutex").take();
    }
}

// -----------------------------------------------------------------------------
// Adapter: foreign MeshTransport -> core::Transport (async_trait).
// -----------------------------------------------------------------------------

struct ForeignTransportAdapter {
    inner: Arc<dyn MeshTransport>,
}

#[async_trait::async_trait]
impl Transport for ForeignTransportAdapter {
    async fn write_to_radio(&self, bytes: &[u8]) -> voicetastic_core::Result<()> {
        let cb = self.inner.clone();
        let payload = bytes.to_vec();
        // The foreign callback is synchronous and may block on platform
        // I/O (BLE GATT write, USB serial flush). Hop to the blocking
        // pool so we don't stall the bridge reactor.
        tokio::task::spawn_blocking(move || cb.write_to_radio(payload))
            .await
            .map_err(|e| voicetastic_core::Error::Other(format!("write_to_radio join: {e}")))?;
        Ok(())
    }

    async fn disconnect(&self) -> voicetastic_core::Result<()> {
        let cb = self.inner.clone();
        tokio::task::spawn_blocking(move || cb.shutdown())
            .await
            .map_err(|e| voicetastic_core::Error::Other(format!("shutdown join: {e}")))?;
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// MeshService wrapper
// -----------------------------------------------------------------------------

/// Inbound channel capacity. Sized for a worst-case burst (full config
/// download ≈ 30 frames) without blocking the BLE notify callback. Frames
/// are small (≤ MAX_TO_FROM_RADIO_SIZE ≈ 512 B), so the worst-case memory
/// footprint is ~64 KiB.
const INBOUND_CHANNEL_CAPACITY: usize = 128;

/// Listener task handles. We keep them so that `set_*_listener` cleanly
/// replaces a previously-registered listener.
#[derive(Default)]
struct ListenerHandles {
    state: Option<JoinHandle<()>>,
    text: Option<JoinHandle<()>>,
    data: Option<JoinHandle<()>>,
    queue: Option<JoinHandle<()>>,
    config: Option<JoinHandle<()>>,
}

impl ListenerHandles {
    fn replace_state(&mut self, h: JoinHandle<()>) {
        if let Some(prev) = self.state.replace(h) {
            prev.abort();
        }
    }
    fn replace_text(&mut self, h: JoinHandle<()>) {
        if let Some(prev) = self.text.replace(h) {
            prev.abort();
        }
    }
    fn replace_data(&mut self, h: JoinHandle<()>) {
        if let Some(prev) = self.data.replace(h) {
            prev.abort();
        }
    }
    fn replace_queue(&mut self, h: JoinHandle<()>) {
        if let Some(prev) = self.queue.replace(h) {
            prev.abort();
        }
    }
    fn replace_config(&mut self, h: JoinHandle<()>) {
        if let Some(prev) = self.config.replace(h) {
            prev.abort();
        }
    }
    fn abort_all(&mut self) {
        for h in [
            self.state.take(),
            self.text.take(),
            self.data.take(),
            self.queue.take(),
            self.config.take(),
        ]
        .into_iter()
        .flatten()
        {
            h.abort();
        }
    }
}

impl Drop for ListenerHandles {
    fn drop(&mut self) {
        self.abort_all();
    }
}

/// UniFFI-exposed Meshtastic service. One instance per Android process.
pub struct MeshService {
    core: CoreMeshService,
    listeners: StdMutex<ListenerHandles>,
    /// Lazily-constructed shared outbound voice pipeline. First call
    /// to `voice_sender()` builds it and spawns its NACK-listener task;
    /// later calls return the same handle.
    voice_sender: std::sync::OnceLock<Arc<crate::VoiceSender>>,
}

impl MeshService {
    /// Build a fresh service. `core::MeshService::new` is async only
    /// because it constructs a few tokio primitives; we block on it once
    /// here so the Kotlin constructor stays synchronous.
    pub fn new() -> Result<Self, MeshServiceError> {
        let core = runtime().block_on(CoreMeshService::new())?;
        Ok(Self {
            core,
            listeners: StdMutex::new(ListenerHandles::default()),
            voice_sender: std::sync::OnceLock::new(),
        })
    }

    /// Register the foreign transport and start the inbound pump.
    ///
    /// Returns the [`MeshTransportSink`] the foreign code calls when a
    /// frame arrives. After this returns, the service will start its
    /// config-burst handshake.
    pub fn connect(
        &self,
        transport: Arc<dyn MeshTransport>,
        settle_delay_ms: u64,
    ) -> Result<Arc<MeshTransportSink>, MeshServiceError> {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(INBOUND_CHANNEL_CAPACITY);
        let sink = Arc::new(MeshTransportSink {
            sender: StdMutex::new(Some(tx)),
        });
        let adapter = Arc::new(ForeignTransportAdapter { inner: transport }) as Arc<dyn Transport>;
        let svc = self.core.clone();
        runtime().block_on(async move {
            svc.connect_with_transport(adapter, rx, Duration::from_millis(settle_delay_ms))
                .await
        })?;
        Ok(sink)
    }

    pub fn disconnect(&self) -> Result<(), MeshServiceError> {
        let svc = self.core.clone();
        runtime().block_on(async move { svc.disconnect().await })?;
        Ok(())
    }

    pub fn refresh_config(&self) -> Result<(), MeshServiceError> {
        let svc = self.core.clone();
        runtime().block_on(async move { svc.refresh_config().await })?;
        Ok(())
    }

    pub fn my_node_num(&self) -> Option<u32> {
        self.core.my_node_num()
    }

    pub fn send_text(
        &self,
        text: String,
        channel: u32,
        dest: Option<u32>,
    ) -> Result<u32, MeshServiceError> {
        let svc = self.core.clone();
        let id = runtime().block_on(async move { svc.send_text(&text, channel, dest).await })?;
        Ok(id)
    }

    pub fn send_data(
        &self,
        portnum: i32,
        payload: Vec<u8>,
        channel: u32,
        dest: Option<u32>,
        want_ack: bool,
    ) -> Result<u32, MeshServiceError> {
        let svc = self.core.clone();
        let id = runtime().block_on(async move {
            svc.send_data(portnum, payload, channel, dest, want_ack)
                .await
        })?;
        Ok(id)
    }

    /// Build a voice message with `voicetastic_core::voice::build_message`
    /// and push the frames through the paced TX worker. Returns the
    /// per-frame packet ids in send order.
    pub fn send_voice(
        &self,
        audio: Vec<u8>,
        cfg: BuildConfig,
        channel: u32,
        dest: Option<u32>,
        pacing_ms: u64,
    ) -> Result<Vec<u32>, MeshServiceError> {
        let encryption = match cfg.channel_psk.as_ref() {
            Some(psk) => Some(v::derive_key(psk, cfg.message_id, cfg.from_node_num)?),
            None => None,
        };
        let core_cfg = v::BuildConfig {
            message_id: cfg.message_id,
            stream_seq: cfg.stream_seq,
            codec: cfg.codec.into(),
            codec_param: cfg.codec_param,
            chunk_size: cfg.chunk_size as usize,
            parity_count: cfg.parity_count,
            last_in_stream: cfg.last_in_stream,
            encryption,
            mac_key: cfg.channel_psk.clone(),
        };
        let message = v::build_message(&audio, &core_cfg)?;
        let svc = self.core.clone();
        let pacing = Duration::from_millis(pacing_ms);
        let ids = runtime()
            .block_on(async move { svc.send_voice(&message, channel, dest, pacing).await })?;
        Ok(ids)
    }

    /// Send an already-encoded `AdminMessage` protobuf payload. Kotlin
    /// builds the message with its geeksville-mesh codegen and passes the
    /// serialized bytes. We decode just enough to re-route into
    /// `MeshService::send_admin`, which handles the `to=my_node_num`
    /// framing.
    pub fn write_admin(&self, admin_proto: Vec<u8>) -> Result<u32, MeshServiceError> {
        let admin = AdminMessage::decode(admin_proto.as_slice()).map_err(|e| {
            MeshServiceError::Protocol {
                error_message: format!("AdminMessage decode: {e}"),
            }
        })?;
        let variant = admin
            .payload_variant
            .ok_or_else(|| MeshServiceError::InvalidArgument {
                error_message: "AdminMessage.payload_variant is empty".into(),
            })?;
        let svc = self.core.clone();
        let id = runtime().block_on(async move { svc.send_admin(variant).await })?;
        Ok(id)
    }

    // -- Listener registration -------------------------------------------------

    pub fn set_state_listener(&self, listener: Arc<dyn MeshStateListener>) {
        let mut rx = self.core.watch_state();
        // Push the current value first so the UI doesn't sit on a stale default.
        listener.on_state((*rx.borrow()).into());
        let handle = runtime().spawn(async move {
            while rx.changed().await.is_ok() {
                let s: MeshConnectionState = (*rx.borrow_and_update()).into();
                listener.on_state(s);
            }
        });
        self.listeners.lock().unwrap().replace_state(handle);
    }

    pub fn set_text_listener(&self, listener: Arc<dyn MeshTextListener>) {
        let mut rx = self.core.subscribe_text();
        let handle = runtime().spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(t) => listener.on_text(t.into()),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "text listener lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        self.listeners.lock().unwrap().replace_text(handle);
    }

    pub fn set_data_listener(&self, listener: Arc<dyn MeshDataListener>) {
        let mut rx = self.core.subscribe_data();
        let handle = runtime().spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(d) => listener.on_data(d.into()),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "data listener lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        self.listeners.lock().unwrap().replace_data(handle);
    }

    pub fn set_queue_listener(&self, listener: Arc<dyn MeshQueueListener>) {
        let mut rx = self.core.subscribe_queue_status();
        let handle = runtime().spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(q) => listener.on_queue_status(q.into()),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "queue listener lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        self.listeners.lock().unwrap().replace_queue(handle);
    }

    pub fn set_config_listener(&self, listener: Arc<dyn MeshConfigListener>) {
        let core = self.core.clone();
        let handle = runtime().spawn(async move {
            // Subscribe to every per-section watch + the config_complete
            // broadcast in a single task. `tokio::select!` lets one
            // listener service all of them without spawning ~10 tasks.
            let mut my_info = core.watch_my_info();
            let mut nodes = core.watch_nodes();
            let mut lora = core.watch_lora_config();
            let mut device = core.watch_device_config();
            let mut position = core.watch_position_config();
            let mut power = core.watch_power_config();
            let mut network = core.watch_network_config();
            let mut display = core.watch_display_config();
            let mut bluetooth = core.watch_bluetooth_config();
            let mut channels = core.watch_channels();
            let mut owner = core.watch_owner();
            let mut metadata = core.watch_metadata();
            let mut complete = core.subscribe_config_complete();

            // Push initial snapshots so the UI doesn't wait for the next change.
            push_my_info(&listener, &my_info.borrow());
            // Delta-tracking for the node map: we only call `on_node_info` for
            // entries that are new or have changed since the last emission.
            //
            // Without this, `push_nodes` re-emitted the *entire* map every
            // time any single node was added or updated. During a config burst
            // of N nodes that produced N*(N+1)/2 cross-FFI calls (210 for 20
            // nodes) instead of N. The extra calls also triggered an equal
            // number of `_nodes.value` StateFlow emissions + UI recompositions
            // on the Kotlin side.
            let mut pushed_nodes: std::collections::HashMap<u32, NodeInfo> = {
                let snap = nodes.borrow();
                for ni in snap.values() {
                    listener.on_node_info(encode(ni));
                }
                snap.clone()
            };
            push_config_variant(
                &listener,
                lora.borrow().as_ref().map(|c| config::PayloadVariant::Lora(c.clone())),
            );
            push_config_variant(
                &listener,
                device
                    .borrow()
                    .as_ref()
                    .map(|c| config::PayloadVariant::Device(c.clone())),
            );
            push_config_variant(
                &listener,
                position
                    .borrow()
                    .as_ref()
                    .map(|c| config::PayloadVariant::Position(*c)),
            );
            push_config_variant(
                &listener,
                power.borrow().as_ref().map(|c| config::PayloadVariant::Power(*c)),
            );
            push_config_variant(
                &listener,
                network
                    .borrow()
                    .as_ref()
                    .map(|c| config::PayloadVariant::Network(c.clone())),
            );
            push_config_variant(
                &listener,
                display
                    .borrow()
                    .as_ref()
                    .map(|c| config::PayloadVariant::Display(*c)),
            );
            push_config_variant(
                &listener,
                bluetooth
                    .borrow()
                    .as_ref()
                    .map(|c| config::PayloadVariant::Bluetooth(*c)),
            );
            push_channels(&listener, &channels.borrow());
            push_owner(&listener, &owner.borrow());
            push_metadata(&listener, &metadata.borrow());

            loop {
                tokio::select! {
                    biased;
                    Ok(_) = my_info.changed() => push_my_info(&listener, &my_info.borrow_and_update()),
                    Ok(_) = nodes.changed() => {
                        // Only emit nodes that are new or have changed since the
                        // last call. See the `pushed_nodes` comment above.
                        let snap = nodes.borrow_and_update();
                        for (num, ni) in snap.iter() {
                            if pushed_nodes.get(num) != Some(ni) {
                                listener.on_node_info(encode(ni));
                            }
                        }
                        pushed_nodes = snap.clone();
                    },
                    Ok(_) = lora.changed() => push_config_variant(
                        &listener,
                        lora.borrow_and_update()
                            .as_ref()
                            .map(|c| config::PayloadVariant::Lora(c.clone())),
                    ),
                    Ok(_) = device.changed() => push_config_variant(
                        &listener,
                        device.borrow_and_update()
                            .as_ref()
                            .map(|c| config::PayloadVariant::Device(c.clone())),
                    ),
                    Ok(_) = position.changed() => push_config_variant(
                        &listener,
                        position.borrow_and_update()
                            .as_ref()
                            .map(|c| config::PayloadVariant::Position(*c)),
                    ),
                    Ok(_) = power.changed() => push_config_variant(
                        &listener,
                        power.borrow_and_update()
                            .as_ref()
                            .map(|c| config::PayloadVariant::Power(*c)),
                    ),
                    Ok(_) = network.changed() => push_config_variant(
                        &listener,
                        network.borrow_and_update()
                            .as_ref()
                            .map(|c| config::PayloadVariant::Network(c.clone())),
                    ),
                    Ok(_) = display.changed() => push_config_variant(
                        &listener,
                        display.borrow_and_update()
                            .as_ref()
                            .map(|c| config::PayloadVariant::Display(*c)),
                    ),
                    Ok(_) = bluetooth.changed() => push_config_variant(
                        &listener,
                        bluetooth.borrow_and_update()
                            .as_ref()
                            .map(|c| config::PayloadVariant::Bluetooth(*c)),
                    ),
                    Ok(_) = channels.changed() => push_channels(&listener, &channels.borrow_and_update()),
                    Ok(_) = owner.changed() => push_owner(&listener, &owner.borrow_and_update()),
                    Ok(_) = metadata.changed() => push_metadata(&listener, &metadata.borrow_and_update()),
                    res = complete.recv() => match res {
                        Ok(nonce) => listener.on_config_complete(nonce),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    else => break,
                }
            }
        });
        self.listeners.lock().unwrap().replace_config(handle);
    }

    /// Return the shared [`crate::VoiceSender`] bound to this service.
    /// First call constructs it (and spawns its NACK-listener task);
    /// subsequent calls return the same instance. Cheap to call from
    /// any thread — `OnceLock::get_or_init` is racing-safe.
    pub fn voice_sender(&self) -> Arc<crate::VoiceSender> {
        self.voice_sender
            .get_or_init(|| Arc::new(crate::VoiceSender::new(self.core.clone())))
            .clone()
    }
}

// -----------------------------------------------------------------------------
// Helpers — encode a snapshot value as protobuf bytes and forward to the
// matching `MeshConfigListener` callback.
// -----------------------------------------------------------------------------

fn encode<M: prost::Message>(msg: &M) -> Vec<u8> {
    let mut buf = Vec::with_capacity(msg.encoded_len());
    // Encoding into a Vec that we just sized is infallible.
    msg.encode(&mut buf).expect("prost encode into Vec");
    buf
}

fn push_my_info(listener: &Arc<dyn MeshConfigListener>, snap: &Option<MyNodeInfo>) {
    if let Some(info) = snap {
        listener.on_my_info(encode(info));
    }
}

fn push_config_variant(
    listener: &Arc<dyn MeshConfigListener>,
    variant: Option<config::PayloadVariant>,
) {
    if let Some(v) = variant {
        let cfg = Config {
            payload_variant: Some(v),
        };
        listener.on_config(encode(&cfg));
    }
}

fn push_channels(listener: &Arc<dyn MeshConfigListener>, snap: &[Channel]) {
    for ch in snap {
        listener.on_channel(encode(ch));
    }
}

fn push_owner(listener: &Arc<dyn MeshConfigListener>, snap: &Option<User>) {
    if let Some(u) = snap {
        listener.on_owner(encode(u));
    }
}

fn push_metadata(
    listener: &Arc<dyn MeshConfigListener>,
    snap: &Option<voicetastic_core::proto::DeviceMetadata>,
) {
    if let Some(m) = snap {
        listener.on_metadata(encode(m));
    }
}

// -----------------------------------------------------------------------------
// Free helpers exposed to Kotlin
// -----------------------------------------------------------------------------

/// `aabbccdd` → `!aabbccdd` text id for the given node number.
pub fn node_num_to_id(num: u32) -> String {
    voicetastic_core::ids::node_num_to_id(num)
}

/// Parse a `!aabbccdd` text id back into a node number; returns `None`
/// for any malformed id (core returns `Err`, which we collapse to
/// `None` to match the more permissive Kotlin call-site idiom).
pub fn node_id_to_num(id: String) -> Option<u32> {
    voicetastic_core::ids::node_id_to_num(&id).ok()
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex as StdMutex, Mutex};

    /// Test-only [`MeshTransport`] that captures everything Rust writes
    /// and lets the test simulate inbound frames via the returned sink.
    struct LoopbackTransport {
        writes: StdMutex<Vec<Vec<u8>>>,
        closed: AtomicUsize,
    }
    impl LoopbackTransport {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                writes: StdMutex::new(Vec::new()),
                closed: AtomicUsize::new(0),
            })
        }
    }
    impl MeshTransport for LoopbackTransport {
        fn write_to_radio(&self, bytes: Vec<u8>) {
            self.writes.lock().unwrap().push(bytes);
        }
        fn shutdown(&self) {
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct StateRecorder(Mutex<Vec<MeshConnectionState>>);
    impl MeshStateListener for StateRecorder {
        fn on_state(&self, state: MeshConnectionState) {
            self.0.lock().unwrap().push(state);
        }
    }

    #[test]
    fn connect_registers_transport_and_writes_want_config() {
        let svc = MeshService::new().expect("new");
        let transport = LoopbackTransport::new();
        let sink = svc
            .connect(transport.clone() as Arc<dyn MeshTransport>, 0)
            .expect("connect");
        // connect_with_transport sends WantConfigId immediately when
        // settle_delay == 0; with the spawn_blocking hop the write should
        // be observable within a short bounded wait.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while transport.writes.lock().unwrap().is_empty() {
            if std::time::Instant::now() > deadline {
                panic!("transport never received WantConfigId");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Closing the sink should let the inbound task wind down without
        // panicking. We don't assert on `disconnect()` here because that
        // would try to issue another write through the (already-closed)
        // transport.
        sink.shutdown();
    }

    #[test]
    fn state_listener_receives_initial_snapshot() {
        let svc = MeshService::new().expect("new");
        let recorder = Arc::new(StateRecorder(Mutex::new(Vec::new())));
        svc.set_state_listener(recorder.clone() as Arc<dyn MeshStateListener>);
        // The synchronous initial push should already be visible.
        let states = recorder.0.lock().unwrap().clone();
        assert_eq!(states, vec![MeshConnectionState::Disconnected]);
    }

    #[test]
    fn node_id_helpers_round_trip() {
        let id = node_num_to_id(0xdead_beef);
        assert_eq!(id, "!deadbeef");
        assert_eq!(node_id_to_num(id), Some(0xdead_beef));
        assert_eq!(node_id_to_num("garbage".into()), None);
    }
}
