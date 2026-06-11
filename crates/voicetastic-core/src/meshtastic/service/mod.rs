//! High-level façade over a [`crate::Transport`] and [`crate::proto`].
//!
//! [`MeshtasticService`] owns an `Arc<dyn Transport>`, sends a `WantConfigId`
//! handshake on connect, fans incoming `FromRadio` messages out to typed
//! observers, and exposes outbound helpers (`send_text`, `send_data`).
//!
//! Two convenience constructors wrap the in-tree built-in transports:
//! [`MeshtasticService::connect_by_address`] (BLE, requires the `ble-btleplug`
//! feature) and [`MeshtasticService::connect_by_serial`] (USB-serial, requires
//! `serial-tokio`). External transports — e.g. an Android JNI bridge —
//! plug in via [`MeshtasticService::connect_with_transport`].

mod inbound;
mod outbound;
pub mod protocol;
mod types;
mod voice_tx;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, Notify, broadcast, mpsc, watch};
use tracing::{debug, warn};

/// Maximum time we wait for the device to finish its config burst (i.e. for
/// `ConnectionState` to leave `Configuring`). If the radio never sends
/// `ConfigCompleteId`, we revert to `Connected` so the UI isn't stranded.
const CONFIG_BURST_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(feature = "ble-btleplug")]
use crate::ble::{BleManager, Connection, DiscoveredDevice};
#[cfg(feature = "ble-btleplug")]
use crate::error::Error;
use crate::error::Result;
use crate::node::NodeId;
use crate::proto::{
    Channel, DeviceMetadata, MyNodeInfo, NodeInfo, User,
    config::{
        BluetoothConfig, DeviceConfig, DisplayConfig, LoRaConfig, NetworkConfig, PositionConfig,
        PowerConfig,
    },
    to_radio,
};
#[cfg(feature = "serial-tokio")]
use crate::serial::SerialConnection;
use crate::transport::Transport;
use crate::voice::ModemPreset;
use crate::voice::types::VoiceData;

pub use types::{
    ConnectionState, IncomingData, IncomingText, QueueStatusEvent, node_display_name,
    node_long_name, node_short_name,
};

/// Convert Meshtastic `LoRaConfig.modem_preset` proto integer to `ModemPreset`.
/// Returns `None` for unknown values; callers should fall back to safe defaults.
pub fn modem_preset_from_proto(value: i32) -> Option<ModemPreset> {
    Some(match value {
        0 => ModemPreset::LongFast,
        1 => ModemPreset::LongSlow,
        2 => ModemPreset::VeryLongSlow,
        3 => ModemPreset::MediumSlow,
        4 => ModemPreset::MediumFast,
        5 => ModemPreset::ShortSlow,
        6 => ModemPreset::ShortFast,
        7 => ModemPreset::LongModerate,
        8 => ModemPreset::ShortTurbo,
        _ => return None,
    })
}

/// What to reconnect to, captured at first-connect time and consumed by
/// the auto-reconnect watcher after a disconnect. Cleared by an explicit
/// `disconnect()` so a user-initiated teardown doesn't immediately
/// bounce back up.
#[derive(Debug, Clone)]
enum ReconnectConfig {
    #[cfg(feature = "serial-tokio")]
    Serial { path: String, baud: u32 },
    #[cfg(feature = "ble-btleplug")]
    Ble { address: String },
}

/// Backoff bounds for the auto-reconnect watcher.
#[cfg(any(feature = "serial-tokio", feature = "ble-btleplug"))]
const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);
#[cfg(any(feature = "serial-tokio", feature = "ble-btleplug"))]
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
/// A connection is considered "stable" (and the backoff campaign reset) if it
/// stayed up for at least this long before dropping. Below this threshold the
/// device is treated as flapping and the previous campaign's longer delays are
/// continued, so a pathological device can't keep hammering at 1 s forever.
#[cfg(any(feature = "serial-tokio", feature = "ble-btleplug"))]
const RECONNECT_STABILITY_WINDOW: Duration = Duration::from_secs(60);

/// Returns `true` if a connection established at `connected_at` was stable
/// long enough to justify resetting the reconnect backoff campaign.
#[cfg(any(feature = "serial-tokio", feature = "ble-btleplug"))]
fn campaign_should_reset(
    connected_at: Option<std::time::Instant>,
    window: Duration,
) -> bool {
    connected_at.is_some_and(|t| t.elapsed() >= window)
}

/// Service handle. Cheap to clone — internally `Arc`'d.
#[derive(Clone)]
pub struct MeshtasticService {
    inner: Arc<Inner>,
}

struct Inner {
    #[cfg(feature = "ble-btleplug")]
    ble: tokio::sync::OnceCell<BleManager>,
    transport: Mutex<Option<Arc<dyn Transport>>>,
    /// Cached snapshot of [`Transport::max_tx_payload`] from whichever
    /// transport is currently connected. Read on the sync hot path by
    /// [`MeshtasticService::max_voice_body_size`] (which can't `.await`
    /// the `transport` mutex) and refreshed at every connect /
    /// disconnect site. Defaults to [`usize::MAX`] so the absence of a
    /// transport doesn't artificially throttle chunk sizing in tests.
    transport_max_tx_payload: AtomicUsize,
    /// Monotonic connection generation, bumped on every connect and
    /// disconnect. Each inbound reader task captures the generation it was
    /// spawned for and only performs teardown (clearing the transport slot,
    /// setting `Disconnected`, auto-reconnect) while it still matches. This
    /// stops a stale reader from a superseded connection from tearing down a
    /// healthy newer transport when its own inbound stream finally ends.
    conn_generation: AtomicU64,
    /// Monotonic want-config-round generation, bumped each time a config
    /// watchdog is armed. A watchdog captures its round at spawn and bails if
    /// a newer round has since started, so overlapping watchdogs (connect +
    /// `refresh_config` + reconnect) can't revert a progressing connection.
    config_generation: AtomicU64,
    /// Canonical device config/identity snapshot (sans-IO). The watch
    /// channels below mirror it for the subscriber API; this is the single
    /// source of truth, and the one a non-tokio (browser) driver would read.
    state: parking_lot::Mutex<protocol::ProtocolState>,
    state_tx: watch::Sender<ConnectionState>,
    my_info_tx: watch::Sender<Option<MyNodeInfo>>,
    nodes_tx: watch::Sender<HashMap<u32, NodeInfo>>,
    config_complete_tx: broadcast::Sender<u32>,
    incoming_text_tx: broadcast::Sender<IncomingText>,
    incoming_data_tx: broadcast::Sender<IncomingData>,
    /// Monotonic packet id, seeded from the OS RNG so flood-routing
    /// deduplication on the mesh doesn't clash with recently-seen packets.
    /// `AtomicU32` rather than `Mutex<u32>` so [`outbound::next_id`] can
    /// allocate ids without taking an async lock on the hot send path.
    next_packet_id: AtomicU32,
    /// Producer end of the serialized voice TX queue. The worker is
    /// spawned in [`MeshtasticService::new`] and holds a `Weak<Inner>` so it
    /// shuts down when the last external [`MeshtasticService`] clone drops.
    voice_tx: mpsc::Sender<voice_tx::VoiceTxItem>,
    /// Latest firmware-reported outbound queue snapshot. Updated from
    /// `FromRadio::QueueStatus` events. `free` defaults to `u32::MAX`
    /// (i.e. "unknown / assume room") until the first report arrives so
    /// we don't gate the very first send on a value we've never seen.
    /// `radio_queue_notify` is pulsed on every update so the voice TX
    /// worker can wake up as soon as the firmware drains its queue.
    pub(super) radio_queue_free: parking_lot::Mutex<u32>,
    pub(super) radio_queue_notify: Arc<Notify>,
    /// Broadcast of each raw `QueueStatus` event the firmware emits.
    /// Consumers (e.g. the chat UI) subscribe to track when individual
    /// outbound packets have actually been transmitted on air.
    pub(super) queue_status_tx: broadcast::Sender<types::QueueStatusEvent>,
    /// Protocol-filtered inbound voice data (port + version checked).
    pub(super) voice_data_tx: broadcast::Sender<VoiceData>,
    /// Fan-out of every ack/nak event the firmware reports, keyed by the
    /// originating packet id. The `pending_acks` oneshot path below is
    /// the synchronous "wait for a specific packet" API; this broadcast
    /// is the per-event firehose that callers (Android Kotlin bindings,
    /// future delivery-icon UI) subscribe to without having to register
    /// per-id slots in advance.
    pub(super) ack_event_tx: broadcast::Sender<(u32, crate::meshtastic::ack::AckResult)>,
    /// Outbound packets awaiting their firmware-reported delivery ack.
    /// Keyed by the packet id; populated by `send_*_tracked` before the
    /// send, drained by the inbound `Routing` handler. Entries whose
    /// `AckHandle` has been dropped without resolving leak until the
    /// next `register_ack` call sweeps them (see [`Self::register_ack`]).
    pending_acks: parking_lot::Mutex<
        HashMap<u32, tokio::sync::oneshot::Sender<crate::meshtastic::ack::AckResult>>,
    >,
    // Configuration sections, each updated when the device emits its
    // matching `Config` chunk during the want-config burst.
    pub(super) lora_tx: watch::Sender<Option<LoRaConfig>>,
    pub(super) device_tx: watch::Sender<Option<DeviceConfig>>,
    pub(super) position_tx: watch::Sender<Option<PositionConfig>>,
    pub(super) power_tx: watch::Sender<Option<PowerConfig>>,
    pub(super) network_tx: watch::Sender<Option<NetworkConfig>>,
    pub(super) display_tx: watch::Sender<Option<DisplayConfig>>,
    pub(super) bluetooth_tx: watch::Sender<Option<BluetoothConfig>>,
    /// MQTT module-config snapshot, when the firmware has reported one.
    /// Tracked independently of the seven `Config` sections because the
    /// firmware emits it through `FromRadio::ModuleConfig`, not
    /// `FromRadio::Config`.
    pub(super) mqtt_tx: watch::Sender<Option<crate::proto::module_config::MqttConfig>>,
    pub(super) channels_tx: watch::Sender<Vec<Channel>>,
    pub(super) owner_tx: watch::Sender<Option<User>>,
    pub(super) metadata_tx: watch::Sender<Option<DeviceMetadata>>,
    /// What to reconnect to when the inbound stream drops. Set by
    /// [`MeshtasticService::connect_by_serial_baud`] and
    /// [`MeshtasticService::connect_by_address`]; cleared by an explicit
    /// [`MeshtasticService::disconnect`]. `None` means reconnect is not
    /// configured (e.g. caller used `connect_with_transport` directly).
    pub(super) reconnect_config: Mutex<Option<ReconnectConfig>>,
    /// Dedup ring buffer for host-side PKC DM decryption. Stores the 64 most
    /// recently decrypted `(from, packet_id)` pairs. Before decrypting a
    /// rescued PKC DM the decoder checks this set; after a successful decrypt
    /// it inserts the pair and evicts the oldest entry if needed. Guards
    /// against firmware flood-retransmits causing the same plaintext DM to
    /// be reported to the app multiple times. Kept separate from
    /// `state` so it can be accessed from `try_pkc_decrypt` while
    /// `state` is already locked.
    pub(super) pkc_seen: parking_lot::Mutex<std::collections::VecDeque<(u32, u32)>>,
    /// Notified when the silence probe triggers a reconnect. A dedicated
    /// watcher task (spawned once in [`MeshtasticService::new`]) consumes this
    /// and calls [`MeshtasticService::connect_by_serial_baud`] so the reconnect
    /// doesn't create a `tokio::spawn` recursion through
    /// [`connect_with_transport`].
    pub(super) reconnect_request: Arc<Notify>,
}

impl MeshtasticService {
    pub async fn new() -> Result<Self> {
        let (state_tx, _) = watch::channel(ConnectionState::Disconnected);
        let (my_info_tx, _) = watch::channel(None);
        let (nodes_tx, _) = watch::channel(HashMap::new());
        let (config_complete_tx, _) = broadcast::channel(8);
        let (incoming_text_tx, _) = broadcast::channel(64);
        let (incoming_data_tx, _) = broadcast::channel(1024);
        // Sized generously so a long voice send (≈ data + FEC parity
        // frames, with the firmware sometimes emitting two QS events
        // per packet) can't outrun a momentarily-suspended subscriber
        // and force a `Lagged` error. Each event is small (~16 B), so
        // the ~64 KB worst-case footprint is cheap.
        let (queue_status_tx, _) = broadcast::channel(4096);
        let (voice_data_tx, _) = broadcast::channel(512);
        let (ack_event_tx, _) = broadcast::channel(256);
        let (lora_tx, _) = watch::channel(None);
        let (device_tx, _) = watch::channel(None);
        let (position_tx, _) = watch::channel(None);
        let (power_tx, _) = watch::channel(None);
        let (network_tx, _) = watch::channel(None);
        let (display_tx, _) = watch::channel(None);
        let (bluetooth_tx, _) = watch::channel(None);
        let (mqtt_tx, _) = watch::channel(None);
        let (channels_tx, _) = watch::channel(Vec::new());
        let (owner_tx, _) = watch::channel(None);
        let (metadata_tx, _) = watch::channel(None);
        // The voice TX queue is bootstrapped here: build the channel,
        // wrap Inner in an Arc, then hand the worker a Weak<Inner> so
        // it can shut down cleanly when the last MeshtasticService clone drops.
        let (voice_tx_send, voice_tx_recv) = tokio::sync::mpsc::channel(voice_tx::QUEUE_CAPACITY);
        let reconnect_request = Arc::new(Notify::new());
        let inner = Arc::new(Inner {
            #[cfg(feature = "ble-btleplug")]
            ble: tokio::sync::OnceCell::new(),
            transport: Mutex::new(None),
            transport_max_tx_payload: AtomicUsize::new(usize::MAX),
            conn_generation: AtomicU64::new(0),
            config_generation: AtomicU64::new(0),
            state: parking_lot::Mutex::new(protocol::ProtocolState::default()),
            state_tx,
            my_info_tx,
            nodes_tx,
            radio_queue_free: parking_lot::Mutex::new(u32::MAX),
            radio_queue_notify: Arc::new(Notify::new()),
            config_complete_tx,
            incoming_text_tx,
            incoming_data_tx,
            // Meshtastic firmware uses packet id for flood-routing
            // deduplication; tiny sequential ids would clash with
            // recently-seen packets, so seed from the OS RNG.
            next_packet_id: AtomicU32::new(
                types::rand_u32()
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "OS RNG failed, using fallback packet id seed");
                        1
                    })
                    .max(1),
            ),
            voice_tx: voice_tx_send,
            queue_status_tx,
            voice_data_tx,
            ack_event_tx,
            pending_acks: parking_lot::Mutex::new(HashMap::new()),
            lora_tx,
            device_tx,
            position_tx,
            power_tx,
            network_tx,
            display_tx,
            bluetooth_tx,
            mqtt_tx,
            channels_tx,
            owner_tx,
            metadata_tx,
            reconnect_config: Mutex::new(None),
            reconnect_request: reconnect_request.clone(),
            pkc_seen: parking_lot::Mutex::new(std::collections::VecDeque::new()),
        });
        // Auto-reconnect watcher: notified by [`try_reconnect`] after the
        // inbound stream drops (transport tore down, silence-probe gave up,
        // BLE link broke). Each notification kicks off an inner backoff
        // loop that retries until either the user manually reconnects, the
        // user manually disconnects (clears `reconnect_config`), or the
        // service is dropped (Weak<Inner> upgrade fails).
        //
        // Lives in a dedicated top-level task rather than being called
        // directly from the reader task inside [`connect_with_transport`],
        // which would create a recursive async type chain the compiler
        // can't prove `Send`.
        //
        // Gated on the transports that can populate `ReconnectConfig`:
        // with both off (the wasm build path), the enum has zero variants,
        // the loop is unreachable, and the spawned task would just block
        // forever on a notify nothing can fire.
        #[cfg(any(feature = "serial-tokio", feature = "ble-btleplug"))]
        {
            let weak = Arc::downgrade(&inner);
            tokio::spawn(async move {
                use super::reconnect::{BleReconnectConfig, BleReconnectPolicy};
                let mut policy = BleReconnectPolicy::new(BleReconnectConfig {
                    initial_delay: RECONNECT_INITIAL_DELAY,
                    backoff_num: 2,
                    backoff_den: 1,
                    max_delay: RECONNECT_MAX_DELAY,
                    max_attempts: Some(10),
                });
                let mut last_connected_at: Option<std::time::Instant> = None;
                loop {
                    reconnect_request.notified().await;
                    let Some(inner) = weak.upgrade() else { return };
                    drop(inner);
                    // Reset backoff if the connection that just dropped had
                    // been stable long enough; otherwise continue the current
                    // campaign's longer delays (anti-flap).
                    if campaign_should_reset(
                        last_connected_at.take(),
                        RECONNECT_STABILITY_WINDOW,
                    ) {
                        policy.reset();
                    }
                    loop {
                        // Check give-up BEFORE sleeping so the user gets the
                        // Disconnected state immediately on the 10th failure
                        // rather than one extra delay later.
                        if policy.should_give_up() {
                            warn!(
                                attempts = policy.attempts(),
                                "auto-reconnect: max attempts reached, giving up"
                            );
                            let Some(inner) = weak.upgrade() else { return };
                            let svc = MeshtasticService { inner };
                            *svc.inner.reconnect_config.lock().await = None;
                            svc.set_state(ConnectionState::Disconnected);
                            break;
                        }
                        let delay = policy.next_delay();
                        tokio::time::sleep(delay).await;
                        let Some(inner) = weak.upgrade() else { return };
                        // Re-check intent after sleep: the user may have
                        // manually reconnected (transport now Some) or
                        // disconnected (config now None).
                        let cfg = inner.reconnect_config.lock().await.clone();
                        if inner.transport.lock().await.is_some() {
                            break;
                        }
                        let Some(cfg) = cfg else { break };
                        let svc = MeshtasticService { inner };
                        let result = match cfg {
                            #[cfg(feature = "serial-tokio")]
                            ReconnectConfig::Serial { path, baud } => {
                                svc.set_state(ConnectionState::Connecting);
                                svc.connect_by_serial_baud_inner(&path, baud).await
                            }
                            #[cfg(feature = "ble-btleplug")]
                            ReconnectConfig::Ble { address } => {
                                svc.set_state(ConnectionState::Connecting);
                                svc.connect_by_address_inner(&address).await
                            }
                        };
                        match result {
                            Ok(()) => {
                                // The user may have disconnected while the
                                // attempt was in flight. If reconnect_config
                                // was cleared, honour it by tearing down the
                                // connection we just brought up.
                                if svc.inner.reconnect_config.lock().await.is_none() {
                                    let _ = svc.disconnect().await;
                                } else {
                                    last_connected_at = Some(std::time::Instant::now());
                                }
                                break;
                            }
                            Err(e) => {
                                warn!(
                                    ?e,
                                    delay_s = delay.as_secs(),
                                    "auto-reconnect failed; retrying"
                                );
                                policy.record_failure();
                            }
                        }
                    }
                }
            });
        }
        voice_tx::spawn_worker(Arc::downgrade(&inner), voice_tx_recv);
        Ok(Self { inner })
    }

    #[cfg(feature = "ble-btleplug")]
    pub async fn scan(&self) -> Result<mpsc::Receiver<DiscoveredDevice>> {
        self.ble().await?.scan().await
    }
    #[cfg(feature = "ble-btleplug")]
    pub async fn stop_scan(&self) -> Result<()> {
        self.ble().await?.stop_scan().await
    }

    pub fn watch_state(&self) -> watch::Receiver<ConnectionState> {
        self.inner.state_tx.subscribe()
    }
    pub fn watch_my_info(&self) -> watch::Receiver<Option<MyNodeInfo>> {
        self.inner.my_info_tx.subscribe()
    }
    pub fn watch_nodes(&self) -> watch::Receiver<HashMap<u32, NodeInfo>> {
        self.inner.nodes_tx.subscribe()
    }
    pub fn subscribe_text(&self) -> broadcast::Receiver<IncomingText> {
        self.inner.incoming_text_tx.subscribe()
    }
    pub fn subscribe_data(&self) -> broadcast::Receiver<IncomingData> {
        self.inner.incoming_data_tx.subscribe()
    }
    /// Subscribe to per-packet ack/nak events as the firmware reports
    /// them. Each event carries `(packet_id, AckResult)`. Use this when
    /// you need delivery status for every outgoing packet (e.g. to flip
    /// a UI delivery-status icon) without registering oneshot waiters
    /// per packet via [`Self::send_text_tracked`].
    pub fn subscribe_acks(&self) -> broadcast::Receiver<(u32, crate::meshtastic::ack::AckResult)> {
        self.inner.ack_event_tx.subscribe()
    }

    /// Subscribe to firmware queue-status events. Each event carries
    /// `(res, free, maxlen, mesh_packet_id)` and is emitted as the radio
    /// queue accepts or drains a packet. Useful for confirming that a
    /// specific outbound packet has been transmitted.
    pub fn subscribe_queue_status(&self) -> broadcast::Receiver<QueueStatusEvent> {
        self.inner.queue_status_tx.subscribe()
    }
    pub fn subscribe_config_complete(&self) -> broadcast::Receiver<u32> {
        self.inner.config_complete_tx.subscribe()
    }

    pub fn watch_lora_config(&self) -> watch::Receiver<Option<LoRaConfig>> {
        self.inner.lora_tx.subscribe()
    }
    pub fn watch_device_config(&self) -> watch::Receiver<Option<DeviceConfig>> {
        self.inner.device_tx.subscribe()
    }
    pub fn watch_position_config(&self) -> watch::Receiver<Option<PositionConfig>> {
        self.inner.position_tx.subscribe()
    }
    pub fn watch_power_config(&self) -> watch::Receiver<Option<PowerConfig>> {
        self.inner.power_tx.subscribe()
    }
    pub fn watch_network_config(&self) -> watch::Receiver<Option<NetworkConfig>> {
        self.inner.network_tx.subscribe()
    }
    pub fn watch_display_config(&self) -> watch::Receiver<Option<DisplayConfig>> {
        self.inner.display_tx.subscribe()
    }
    pub fn watch_bluetooth_config(&self) -> watch::Receiver<Option<BluetoothConfig>> {
        self.inner.bluetooth_tx.subscribe()
    }
    /// Subscribe to the radio's MQTT module-config snapshot. Emits
    /// `None` until the radio reports one (during the want-config
    /// burst) and again after a `refresh_config`.
    pub fn watch_mqtt_config(
        &self,
    ) -> watch::Receiver<Option<crate::proto::module_config::MqttConfig>> {
        self.inner.mqtt_tx.subscribe()
    }
    pub fn watch_channels(&self) -> watch::Receiver<Vec<Channel>> {
        self.inner.channels_tx.subscribe()
    }
    pub fn watch_owner(&self) -> watch::Receiver<Option<User>> {
        self.inner.owner_tx.subscribe()
    }
    pub fn watch_metadata(&self) -> watch::Receiver<Option<DeviceMetadata>> {
        self.inner.metadata_tx.subscribe()
    }

    /// Local node number, if known. Required as `to=` for admin writes.
    pub fn my_node_num(&self) -> Option<u32> {
        self.inner
            .state
            .lock()
            .my_info
            .as_ref()
            .map(|i| i.my_node_num)
    }

    /// Snapshot of the local node's `NodeInfo`, if both `MyNodeInfo` and the
    /// corresponding `NodeInfo` burst entry have been received.
    pub fn self_node(&self) -> Option<NodeInfo> {
        let state = self.inner.state.lock();
        state.self_node().cloned()
    }

    /// Wipe the radio's NodeDB, drop our cached learned-peer map, and
    /// re-request the configuration burst.
    ///
    /// `reset_nodedb()` alone leaves the local `nodes_tx` snapshot intact
    /// (the firmware acks the admin packet but never re-bursts NodeInfo
    /// for an empty NodeDB), so a UI that ran just `reset_nodedb()` would
    /// keep showing the stale peer list. This helper clears the local
    /// snapshot in lockstep and then drives `refresh_config()` so the
    /// config-section caches resync too.
    pub async fn reset_nodedb_and_refresh(&self) -> Result<()> {
        self.reset_nodedb().await?;
        self.inner.state.lock().clear_nodes();
        let _ = self.inner.nodes_tx.send(std::collections::HashMap::new());
        self.refresh_config().await
    }

    /// Re-request the entire configuration burst.
    pub async fn refresh_config(&self) -> Result<()> {
        let prev = *self.inner.state_tx.borrow();
        self.set_state(ConnectionState::Configuring);
        if let Err(e) = self.send_want_config().await {
            // Don't strand the UI in `Configuring` if we can't actually ask.
            // Snapshots are intentionally NOT cleared on failure so the UI
            // continues to show the last-known good values.
            self.set_state(prev);
            return Err(e);
        }
        // Only clear local snapshots after the request was actually sent, so
        // a transport failure doesn't blank out the settings UI. Clear the
        // canonical state first, then mirror the cleared values to the watch
        // channels (the subscriber API).
        self.inner.state.lock().clear_config();
        let _ = self.inner.lora_tx.send(None);
        let _ = self.inner.device_tx.send(None);
        let _ = self.inner.position_tx.send(None);
        let _ = self.inner.power_tx.send(None);
        let _ = self.inner.network_tx.send(None);
        let _ = self.inner.display_tx.send(None);
        let _ = self.inner.bluetooth_tx.send(None);
        let _ = self.inner.mqtt_tx.send(None);
        let _ = self.inner.channels_tx.send(Vec::new());
        self.spawn_config_watchdog();
        Ok(())
    }

    /// Connect using a caller-supplied [`Transport`] and inbound stream.
    ///
    /// This is the transport-agnostic entry point — built-in helpers like
    /// [`Self::connect_by_address`] / [`Self::connect_by_serial`] are thin
    /// wrappers around it, and external consumers (Android JNI bridge, test
    /// loopback, …) can call it directly.
    ///
    /// `inbound` is the stream of decoded `FromRadio` payloads (already
    /// deframed by the transport). `settle_delay` is observed before the
    /// initial `WantConfigId` is sent — pass `Duration::ZERO` if the
    /// underlying transport is already fully ready.
    ///
    /// Once the inbound stream returns `None`, the service is moved to
    /// [`ConnectionState::Disconnected`] and the transport slot cleared.
    pub async fn connect_with_transport(
        &self,
        transport: Arc<dyn Transport>,
        inbound: mpsc::Receiver<Vec<u8>>,
        settle_delay: Duration,
    ) -> Result<()> {
        // Claim a generation number and install the transport under the same
        // lock so both operations are atomic from any reader's perspective.
        // A reader that grabs this lock will either see the old generation
        // (it is the current owner) or the new one (we won the slot) - never
        // an in-between state where the generation advanced but the slot
        // wasn't yet replaced.
        let generation;
        let old_transport;
        {
            let mut slot = self.inner.transport.lock().await;
            generation = self.inner.conn_generation.fetch_add(1, Ordering::SeqCst) + 1;
            self.inner
                .transport_max_tx_payload
                .store(transport.max_tx_payload(), Ordering::Relaxed);
            // Reset the firmware queue snapshot to "assume room" so a stale
            // low-water value left over from the previous session doesn't
            // stall the first voice burst until the next QueueStatus arrives.
            *self.inner.radio_queue_free.lock() = u32::MAX;
            old_transport = slot.replace(transport);
        }
        // Actively shut down any superseded transport so its OS resources
        // (BLE GATT connection, serial port fd) are released promptly rather
        // than waiting for its inbound stream to drain to EOF.
        if let Some(old) = old_transport {
            let _ = old.disconnect().await;
        }
        self.set_state(ConnectionState::Connected);

        let svc = self.clone();
        let mut inbound = inbound;
        tokio::spawn(async move {
            // Probe the device every 60 s of complete silence to verify
            // the read path is still alive. Without this, a serial port
            // whose inbound endpoint stalls (USB buffer overflow, partial
            // frame, …) would leave the read task parked in `read_byte`
            // forever — the app could still write commands but would
            // never see responses or received messages.
            //
            // `send_want_config()` can succeed even when the read path
            // is dead (writes still go through), so we track how many
            // probes have been sent without any inbound data arriving
            // between them. After 2 consecutive silent probes (~180 s:
            // 3 ticks × 60 s because the ≥ check is *before* incrementing)
            // we assume the read path is gone and force a disconnect.
            let mut probe_interval = tokio::time::interval(Duration::from_secs(60));
            probe_interval.reset();
            let mut silent_probes: u32 = 0;
            loop {
                tokio::select! {
                    biased;
                    payload = inbound.recv() => {
                        let Some(payload) = payload else { break };
                        silent_probes = 0;
                        // Data arrived — reset the silence timer so
                        // the next probe doesn't fire for another 60 s.
                        probe_interval.reset();
                        if let Err(e) = svc.handle_from_radio(&payload).await {
                            warn!(?e, "from_radio handler failed");
                        }
                    }
                    _ = probe_interval.tick() => {
                        // No inbound data for 60 s — probe the device.
                        if silent_probes >= 2 {
                            warn!("inbound: no data for ~180 s despite probes, disconnecting");
                            break;
                        }
                        silent_probes += 1;
                        debug!(
                            silent_probes,
                            "inbound probe: no data, sending WantConfigId",
                        );
                        if let Err(e) = svc.send_want_config().await {
                            warn!(?e, "inbound probe send failed, disconnecting");
                            break;
                        }
                        // Probe sent successfully. If the device responds,
                        // `inbound.recv()` will fire before the next tick
                        // and reset silent_probes. If not, the next tick
                        // will see silent_probes >= 2 and disconnect.
                    }
                }
            }
            // Only tear down if we're still the current connection. A newer
            // connect_with_transport (manual re-connect, device switch, or an
            // auto-reconnect race) bumps conn_generation; in that case this
            // stream just ended for an already-superseded transport and the
            // slot now holds the healthy new one, so we must not touch it.
            // The generation is checked while holding the transport slot lock
            // so it can't change between the check and the clear (a competing
            // connect bumps the generation before it takes this same lock).
            {
                let mut slot = svc.inner.transport.lock().await;
                if svc.inner.conn_generation.load(Ordering::SeqCst) != generation {
                    debug!(
                        generation,
                        "inbound reader for superseded connection exiting without teardown"
                    );
                    return;
                }
                *slot = None;
                svc.inner
                    .transport_max_tx_payload
                    .store(usize::MAX, Ordering::Relaxed);
            }
            svc.set_state(ConnectionState::Disconnected);
            // Auto-reconnect so the user doesn't have to manually
            // reconnect after a USB CDC ACM endpoint stall or a BLE drop.
            svc.try_reconnect().await;
        });

        if !settle_delay.is_zero() {
            tokio::time::sleep(settle_delay).await;
        }
        self.set_state(ConnectionState::Configuring);
        if let Err(e) = self.send_want_config().await {
            // Unwind the half-connected state so the caller sees a clean
            // Disconnected status and the transport slot doesn't leak.
            if let Some(t) = self.teardown_generation(generation).await {
                let _ = t.disconnect().await;
            }
            return Err(e);
        }
        self.spawn_config_watchdog();
        Ok(())
    }

    /// Unwind the connection installed by `connect_with_transport` for
    /// `generation`. Bumps `conn_generation` so the spawned inbound reader
    /// skips its own teardown when its stream closes. Returns the transport
    /// that was in the slot so the caller can disconnect it outside the lock.
    async fn teardown_generation(&self, generation: u64) -> Option<Arc<dyn Transport>> {
        let transport = {
            let mut slot = self.inner.transport.lock().await;
            // Only act if we're still the current generation: a concurrent
            // connect_with_transport may have already superseded this one.
            if self.inner.conn_generation.load(Ordering::SeqCst) == generation {
                self.inner.conn_generation.fetch_add(1, Ordering::SeqCst);
                self.inner
                    .transport_max_tx_payload
                    .store(usize::MAX, Ordering::Relaxed);
                slot.take()
            } else {
                None
            }
        };
        self.set_state(ConnectionState::Disconnected);
        transport
    }

    /// Connect to a peripheral by BLE address (`AA:BB:CC:DD:EE:FF`).
    ///
    /// If the device is not currently connected at the OS level we run
    /// the in-process equivalent of `bluetoothctl pair → trust → connect`
    /// (see [`BleManager::prepare_link`]) and retry once. On a stale bond
    /// the pair step auto-recovers via `remove → pair`.
    #[cfg(feature = "ble-btleplug")]
    pub async fn connect_by_address(&self, address: &str) -> Result<()> {
        self.set_state(ConnectionState::Connecting);
        // Remember the address so the auto-reconnect watcher can bring
        // the link back up after a BLE drop (devices flap often).
        *self.inner.reconnect_config.lock().await = Some(ReconnectConfig::Ble {
            address: address.to_string(),
        });
        if let Err(e) = self.connect_by_address_inner(address).await {
            // Revert to Disconnected if nothing else has since changed state
            // (e.g. a concurrent connect that succeeded must not be clobbered).
            if *self.inner.state_tx.borrow() == ConnectionState::Connecting {
                self.set_state(ConnectionState::Disconnected);
            }
            return Err(e);
        }
        Ok(())
    }

    #[cfg(feature = "ble-btleplug")]
    async fn connect_by_address_inner(&self, address: &str) -> Result<()> {
        let ble = self.ble().await?;
        let peripheral = ble.peripheral_by_address(address).await?;
        let conn = match Connection::open(peripheral).await {
            Ok(c) => Arc::new(c),
            Err(Error::NotConnected) => {
                // Bring the link up the same way `bluetoothctl` would,
                // then look the peripheral up again (BlueZ may have
                // re-issued the DBus object path during pairing).
                ble.prepare_link(address).await?;
                let peripheral = ble.peripheral_by_address(address).await?;
                Arc::new(Connection::open(peripheral).await?)
            }
            Err(e) => return Err(e),
        };
        // `subscribe_inbound` already drains on every notify and on the safety
        // poll, so we must NOT drain here too: btleplug serialises GATT reads
        // per peripheral, but issuing concurrent drains would still race for
        // the FROMRADIO queue ordering.
        let inbound = conn.clone().subscribe_inbound().await?;
        // No settle delay: we attach to a link that the operating system has
        // already opened and `Connection::open` has already validated
        // (services resolved, FROMNUM subscribed). Meshtastic firmware
        // starts a short "phone API session" timer the moment the LE link
        // comes up; if we sleep here we risk that timer firing before our
        // `WantConfigId` lands.
        self.connect_with_transport(conn as Arc<dyn Transport>, inbound, Duration::ZERO)
            .await
    }

    /// Connect to a device by serial port path (e.g. `/dev/ttyUSB0`).
    #[cfg(feature = "serial-tokio")]
    pub async fn connect_by_serial(&self, path: &str) -> Result<()> {
        self.connect_by_serial_baud(path, crate::serial::DEFAULT_BAUD)
            .await
    }

    /// Connect to a device by serial port path with a custom baud rate.
    #[cfg(feature = "serial-tokio")]
    pub async fn connect_by_serial_baud(&self, path: &str, baud: u32) -> Result<()> {
        self.set_state(ConnectionState::Connecting);
        // Remember the path so the silence-probe task can auto-reconnect
        // when the read path stalls (USB CDC ACM endpoint stall, …).
        *self.inner.reconnect_config.lock().await = Some(ReconnectConfig::Serial {
            path: path.to_string(),
            baud,
        });
        if let Err(e) = self.connect_by_serial_baud_inner(path, baud).await {
            if *self.inner.state_tx.borrow() == ConnectionState::Connecting {
                self.set_state(ConnectionState::Disconnected);
            }
            return Err(e);
        }
        Ok(())
    }

    #[cfg(feature = "serial-tokio")]
    async fn connect_by_serial_baud_inner(&self, path: &str, baud: u32) -> Result<()> {
        let serial = Arc::new(SerialConnection::open(path, baud).await?);
        let inbound = serial.subscribe_inbound().await?;
        // Serial port is fully ready after `open` — no settle delay needed.
        self.connect_with_transport(serial as Arc<dyn Transport>, inbound, Duration::ZERO)
            .await
    }

    /// Notify the watcher to auto-reconnect after the inbound stream
    /// drops (silence probe gave up for serial, BLE link broke, …).
    /// No-op when the user disconnected manually or used
    /// [`Self::connect_with_transport`] directly (no `reconnect_config`).
    async fn try_reconnect(&self) {
        if self.inner.reconnect_config.lock().await.is_none() {
            return;
        }
        self.inner.reconnect_request.notify_one();
    }

    pub async fn disconnect(&self) -> Result<()> {
        // Clear the reconnect config so the watcher doesn't auto-reconnect
        // after a user-initiated disconnect. The silence-probe path also
        // calls disconnect, but it calls try_reconnect_serial separately.
        *self.inner.reconnect_config.lock().await = None;
        let transport = {
            let mut slot = self.inner.transport.lock().await;
            // Bump the generation so the reader for this transport skips its
            // own teardown when its inbound stream ends: we do the teardown
            // synchronously here, and an unguarded reader would otherwise fire
            // a spurious reconnect afterwards.
            self.inner.conn_generation.fetch_add(1, Ordering::SeqCst);
            self.inner
                .transport_max_tx_payload
                .store(usize::MAX, Ordering::Relaxed);
            slot.take()
        };
        if let Some(t) = transport {
            let _ = self
                .send_to_radio_via(t.as_ref(), to_radio::PayloadVariant::Disconnect(true))
                .await;
            t.disconnect().await?;
        }
        self.set_state(ConnectionState::Disconnected);
        Ok(())
    }

    /// Active-scan for advertising Meshtastic devices, including those that
    /// are **not** currently paired with this host. Intended for an
    /// in-app "pair new device" flow; the everyday device picker should
    /// keep using the OS-only [`Self::scan`].
    #[cfg(feature = "ble-btleplug")]
    pub async fn discover_pairable(&self, timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
        self.ble().await?.discover_pairable(timeout).await
    }

    /// Take ownership of the BlueZ pairing-prompt receiver. Returns
    /// `None` if the agent failed to register (e.g. another process is
    /// already the system default agent with a higher-priority hold),
    /// or if the receiver has already been taken.
    ///
    /// Only one consumer can hold the receiver at a time — typically
    /// the GUI's modal-dialog task.
    #[cfg(all(feature = "ble-btleplug", target_os = "linux"))]
    pub async fn pairing_prompts(&self) -> Option<mpsc::Receiver<crate::pairing::PairingPrompt>> {
        match self.ble().await {
            Ok(ble) => ble.take_pairing_prompts().await,
            Err(_) => None,
        }
    }

    pub(super) fn set_state(&self, state: ConnectionState) {
        let _ = self.inner.state_tx.send(state);
    }

    /// Lazy-init the BLE adapter on first use. Lets `MeshtasticService::new()`
    /// succeed on machines without Bluetooth (serial-only setups, CI hosts,
    /// integration tests). Failures still surface to the caller of any
    /// BLE-touching method.
    #[cfg(feature = "ble-btleplug")]
    async fn ble(&self) -> Result<&BleManager> {
        self.inner
            .ble
            .get_or_try_init(|| async { BleManager::new().await })
            .await
    }

    /// Spawn a one-shot task that reverts `Configuring` to `Connected` if the
    /// device never sends `ConfigCompleteId` within [`CONFIG_BURST_TIMEOUT`].
    /// Re-sends `WantConfigId` once at the first timeout (in case the original
    /// request was dropped) and waits another window before giving up.
    /// Cheap to call repeatedly; tasks self-exit if state has already moved on.
    fn spawn_config_watchdog(&self) {
        let svc = self.clone();
        // Claim a fresh want-config round. If another watchdog is armed after
        // us (a second refresh_config, a reconnect), it bumps this again and
        // we bail at each wake, so only the newest round can act on the state.
        let round = self.inner.config_generation.fetch_add(1, Ordering::SeqCst) + 1;
        // True while this is still the current round AND we're still waiting
        // on the config burst. Guards every state-reverting decision below.
        let still_current = move |svc: &MeshtasticService| {
            svc.inner.config_generation.load(Ordering::SeqCst) == round
                && *svc.inner.state_tx.borrow() == ConnectionState::Configuring
        };
        tokio::spawn(async move {
            tokio::time::sleep(CONFIG_BURST_TIMEOUT).await;
            if !still_current(&svc) {
                return;
            }
            warn!(
                timeout_s = CONFIG_BURST_TIMEOUT.as_secs(),
                "config burst did not complete; retrying WantConfigId once"
            );
            if let Err(e) = svc.send_want_config().await {
                warn!(?e, "config-burst retry send failed; reverting to Connected");
                if still_current(&svc) {
                    svc.set_state(ConnectionState::Connected);
                }
                return;
            }
            tokio::time::sleep(CONFIG_BURST_TIMEOUT).await;
            if still_current(&svc) {
                warn!(
                    timeout_s = CONFIG_BURST_TIMEOUT.as_secs() * 2,
                    "config burst still incomplete after retry; reverting to Connected"
                );
                svc.set_state(ConnectionState::Connected);
            }
        });
    }
}

/// Compile-time assertion that `MeshtasticService` can be cloned across `tokio::spawn`
/// boundaries and shared between threads. Catches refactors that accidentally
/// embed a `!Send` or `!Sync` field (e.g. `Rc`, `RefCell`, raw pointers).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync + Clone + 'static>() {}
    assert_send_sync::<MeshtasticService>();
};

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    //! Service-level tests that don't need real BLE or serial hardware.
    //!
    //! `MeshtasticService::new()` lazy-inits the BLE adapter, so these tests can
    //! exercise inbound decoding and config-burst sequencing on any host.

    use super::*;
    use crate::proto::{
        Config, FromRadio, MyNodeInfo, NodeInfo, Position, User, config, from_radio,
    };
    use prost::Message as _;

    fn encode(variant: from_radio::PayloadVariant) -> Vec<u8> {
        let msg = FromRadio {
            id: 0,
            payload_variant: Some(variant),
        };
        let mut buf = Vec::with_capacity(msg.encoded_len());
        msg.encode(&mut buf).expect("encode");
        buf
    }

    async fn make_service() -> MeshtasticService {
        MeshtasticService::new()
            .await
            .expect("MeshtasticService::new")
    }

    /// Hold one receiver on every watch we inspect: tokio's `watch::Sender::send`
    /// returns `Err` (without updating the cached value) when all receivers
    /// have been dropped. In production the GUI/CLI is always subscribed; in
    /// tests we have to keep the receivers alive ourselves.
    fn keep_alive(svc: &MeshtasticService) -> Vec<Box<dyn std::any::Any + Send>> {
        vec![
            Box::new(svc.watch_state()),
            Box::new(svc.watch_my_info()),
            Box::new(svc.watch_nodes()),
            Box::new(svc.watch_owner()),
            Box::new(svc.watch_lora_config()),
            Box::new(svc.watch_device_config()),
            Box::new(svc.watch_position_config()),
            Box::new(svc.watch_power_config()),
            Box::new(svc.watch_network_config()),
            Box::new(svc.watch_display_config()),
            Box::new(svc.watch_bluetooth_config()),
            Box::new(svc.watch_channels()),
        ]
    }

    #[tokio::test]
    async fn handles_my_info() {
        let svc = make_service().await;
        let _g = keep_alive(&svc);
        let bytes = encode(from_radio::PayloadVariant::MyInfo(MyNodeInfo {
            my_node_num: 0x1234_5678,
            ..Default::default()
        }));
        // Sanity-check that the encode round-trips before exercising the service.
        let decoded = FromRadio::decode(bytes.as_slice()).expect("decode");
        assert!(matches!(
            decoded.payload_variant,
            Some(from_radio::PayloadVariant::MyInfo(ref i)) if i.my_node_num == 0x1234_5678
        ));
        svc.handle_from_radio(&bytes).await.unwrap();
        let info = svc.inner.my_info_tx.borrow().clone().unwrap();
        assert_eq!(info.my_node_num, 0x1234_5678);
        assert_eq!(svc.my_node_num(), Some(0x1234_5678));
    }

    #[tokio::test]
    async fn config_burst_populates_each_section_then_completes() {
        let svc = make_service().await;
        let _g = keep_alive(&svc);
        let mut state_rx = svc.watch_state();
        // No config received yet: every watch should hold None.
        assert!(svc.inner.lora_tx.borrow().is_none());
        assert!(svc.inner.device_tx.borrow().is_none());
        assert!(svc.inner.position_tx.borrow().is_none());
        assert!(svc.inner.power_tx.borrow().is_none());
        assert!(svc.inner.network_tx.borrow().is_none());
        assert!(svc.inner.display_tx.borrow().is_none());
        assert!(svc.inner.bluetooth_tx.borrow().is_none());

        // Walk the burst variants in arbitrary order.
        for variant in [
            config::PayloadVariant::Lora(Default::default()),
            config::PayloadVariant::Device(Default::default()),
            config::PayloadVariant::Position(Default::default()),
            config::PayloadVariant::Power(Default::default()),
            config::PayloadVariant::Network(Default::default()),
            config::PayloadVariant::Display(Default::default()),
            config::PayloadVariant::Bluetooth(Default::default()),
        ] {
            let bytes = encode(from_radio::PayloadVariant::Config(Config {
                payload_variant: Some(variant),
            }));
            svc.handle_from_radio(&bytes).await.unwrap();
        }
        // All seven sections now have a value.
        assert!(svc.inner.lora_tx.borrow().is_some());
        assert!(svc.inner.device_tx.borrow().is_some());
        assert!(svc.inner.position_tx.borrow().is_some());
        assert!(svc.inner.power_tx.borrow().is_some());
        assert!(svc.inner.network_tx.borrow().is_some());
        assert!(svc.inner.display_tx.borrow().is_some());
        assert!(svc.inner.bluetooth_tx.borrow().is_some());

        // Subscribe before sending the terminator so we don't miss the
        // broadcast notification.
        let mut done = svc.subscribe_config_complete();
        let bytes = encode(from_radio::PayloadVariant::ConfigCompleteId(42));
        svc.handle_from_radio(&bytes).await.unwrap();
        assert_eq!(done.try_recv().ok(), Some(42));
        // State machine must have advanced to Ready.
        assert_eq!(*state_rx.borrow_and_update(), ConnectionState::Ready);
    }

    #[tokio::test]
    async fn rejects_out_of_range_position_coordinates() {
        let svc = make_service().await;
        let _g = keep_alive(&svc);
        let bytes = encode(from_radio::PayloadVariant::NodeInfo(NodeInfo {
            num: 7,
            user: None,
            position: Some(Position {
                latitude_i: Some(900_000_001),     // > 90°
                longitude_i: Some(-1_800_000_001), // < -180°
                ..Default::default()
            }),
            ..Default::default()
        }));
        svc.handle_from_radio(&bytes).await.unwrap();
        let nodes = svc.inner.nodes_tx.borrow().clone();
        let pos = nodes.get(&7).unwrap().position.as_ref().unwrap();
        assert_eq!(pos.latitude_i, None);
        assert_eq!(pos.longitude_i, None);
    }

    #[tokio::test]
    async fn keeps_in_range_position_coordinates() {
        let svc = make_service().await;
        let _g = keep_alive(&svc);
        let bytes = encode(from_radio::PayloadVariant::NodeInfo(NodeInfo {
            num: 8,
            position: Some(Position {
                latitude_i: Some(485_000_000), // ~48.5°
                longitude_i: Some(23_400_000), // ~2.34°
                ..Default::default()
            }),
            ..Default::default()
        }));
        svc.handle_from_radio(&bytes).await.unwrap();
        let nodes = svc.inner.nodes_tx.borrow().clone();
        let pos = nodes.get(&8).unwrap().position.as_ref().unwrap();
        assert_eq!(pos.latitude_i, Some(485_000_000));
        assert_eq!(pos.longitude_i, Some(23_400_000));
    }

    #[tokio::test]
    async fn nodeinfo_for_self_publishes_owner() {
        let svc = make_service().await;
        let _g = keep_alive(&svc);
        // First, set our node number.
        let my_bytes = encode(from_radio::PayloadVariant::MyInfo(MyNodeInfo {
            my_node_num: 0xdead_beef,
            ..Default::default()
        }));
        svc.handle_from_radio(&my_bytes).await.unwrap();
        // Then, a NodeInfo from ourselves: owner should propagate.
        let ni_bytes = encode(from_radio::PayloadVariant::NodeInfo(NodeInfo {
            num: 0xdead_beef,
            user: Some(User {
                id: "!deadbeef".into(),
                long_name: "Me".into(),
                short_name: "Me".into(),
                ..Default::default()
            }),
            ..Default::default()
        }));
        svc.handle_from_radio(&ni_bytes).await.unwrap();
        let owner = svc.inner.owner_tx.borrow().clone().unwrap();
        assert_eq!(owner.long_name, "Me");
    }

    /// Smoke test for `reset_nodedb`: with `my_node_num` known but no
    /// transport attached, the call routes through `send_admin` and fails
    /// at the transport step (NotConnected). Confirms the variant is wired
    /// up and the method exists at the public surface.
    #[tokio::test]
    async fn reset_nodedb_without_transport_errors() {
        let svc = make_service().await;
        let _g = keep_alive(&svc);
        let bytes = encode(from_radio::PayloadVariant::MyInfo(MyNodeInfo {
            my_node_num: 0x1234_5678,
            ..Default::default()
        }));
        svc.handle_from_radio(&bytes).await.unwrap();
        assert!(svc.reset_nodedb().await.is_err());
    }

    /// Same smoke test for `remove_node` — routes through `send_admin`,
    /// fails on the missing transport.
    #[tokio::test]
    async fn remove_node_without_transport_errors() {
        let svc = make_service().await;
        let _g = keep_alive(&svc);
        let bytes = encode(from_radio::PayloadVariant::MyInfo(MyNodeInfo {
            my_node_num: 0x1234_5678,
            ..Default::default()
        }));
        svc.handle_from_radio(&bytes).await.unwrap();
        assert!(svc.remove_node(0xDEAD_BEEF).await.is_err());
    }

    #[tokio::test]
    async fn refresh_config_clears_section_snapshots() {
        let svc = make_service().await;
        let _g = keep_alive(&svc);
        // Seed at least one section.
        let bytes = encode(from_radio::PayloadVariant::Config(Config {
            payload_variant: Some(config::PayloadVariant::Lora(Default::default())),
        }));
        svc.handle_from_radio(&bytes).await.unwrap();
        assert!(svc.inner.lora_tx.borrow().is_some());

        // No transport: send_want_config fails and refresh_config returns
        // Err. In that case we keep the previous snapshots so the UI is not
        // blanked by a transient transport hiccup.
        let prev_state = *svc.inner.state_tx.borrow();
        assert!(svc.refresh_config().await.is_err());
        assert!(
            svc.inner.lora_tx.borrow().is_some(),
            "snapshots must survive a failed refresh"
        );
        assert_eq!(
            *svc.inner.state_tx.borrow(),
            prev_state,
            "state must revert when refresh fails"
        );
    }

    // -------------------------------------------------------------------------
    // campaign_should_reset (3.B stability window)
    // -------------------------------------------------------------------------

    #[cfg(any(feature = "serial-tokio", feature = "ble-btleplug"))]
    #[test]
    fn campaign_should_reset_after_stability_window() {
        let window = Duration::from_millis(100);
        // No prior connect: don't reset.
        assert!(!campaign_should_reset(None, window));
        // Connected just now: not stable yet.
        assert!(!campaign_should_reset(Some(std::time::Instant::now()), window));
        // Connected long enough ago: reset.
        let old = std::time::Instant::now()
            .checked_sub(window + Duration::from_millis(1))
            .unwrap();
        assert!(campaign_should_reset(Some(old), window));
    }

    // -------------------------------------------------------------------------
    // MockTransport - lifecycle tests (M1, M2, M6)
    // -------------------------------------------------------------------------

    use std::sync::atomic::AtomicUsize;

    struct MockTransport {
        disconnect_count: Arc<AtomicUsize>,
        fail_writes: bool,
    }

    #[async_trait::async_trait]
    impl Transport for MockTransport {
        async fn write_to_radio(&self, _bytes: &[u8]) -> crate::error::Result<()> {
            if self.fail_writes {
                return Err(crate::error::Error::Other(
                    "mock write failure".to_string(),
                ));
            }
            Ok(())
        }

        async fn disconnect(&self) -> crate::error::Result<()> {
            self.disconnect_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn make_mock(fail: bool) -> (Arc<MockTransport>, Arc<AtomicUsize>) {
        let dc = Arc::new(AtomicUsize::new(0));
        let t = Arc::new(MockTransport {
            disconnect_count: dc.clone(),
            fail_writes: fail,
        });
        (t, dc)
    }

    #[tokio::test]
    async fn connect_failure_unwinds_transport_and_state() {
        let svc = make_service().await;
        let _g = keep_alive(&svc);
        let (t, dc) = make_mock(true); // write_to_radio always fails
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let result = svc
            .connect_with_transport(t as Arc<dyn Transport>, rx, Duration::ZERO)
            .await;
        assert!(result.is_err(), "expected Err from failing transport");
        // Slot must be cleared and state must be Disconnected.
        assert!(
            svc.inner.transport.lock().await.is_none(),
            "transport slot must be empty after failure"
        );
        assert_eq!(
            *svc.inner.state_tx.borrow(),
            ConnectionState::Disconnected,
            "state must be Disconnected after failed connect"
        );
        // The transport must have been disconnected during unwind.
        assert_eq!(
            dc.load(Ordering::Relaxed),
            1,
            "disconnect() must be called once during failure unwind"
        );
    }

    #[tokio::test]
    async fn superseding_connect_disconnects_previous_transport() {
        let svc = make_service().await;
        let _g = keep_alive(&svc);

        let (t1, dc1) = make_mock(false);
        let (_tx1, rx1) = tokio::sync::mpsc::channel(1);
        svc.connect_with_transport(t1 as Arc<dyn Transport>, rx1, Duration::ZERO)
            .await
            .expect("first connect must succeed");

        let (t2, _dc2) = make_mock(false);
        let (_tx2, rx2) = tokio::sync::mpsc::channel(1);
        svc.connect_with_transport(t2 as Arc<dyn Transport>, rx2, Duration::ZERO)
            .await
            .expect("second connect must succeed");

        // T1 must have been actively disconnected when T2 took the slot.
        assert_eq!(
            dc1.load(Ordering::Relaxed),
            1,
            "superseded transport must be disconnected"
        );
    }

    #[tokio::test]
    async fn stale_reader_does_not_tear_down_new_connection() {
        let svc = make_service().await;
        let _g = keep_alive(&svc);

        let (t1, _dc1) = make_mock(false);
        let (tx1, rx1) = tokio::sync::mpsc::channel(1);
        svc.connect_with_transport(t1 as Arc<dyn Transport>, rx1, Duration::ZERO)
            .await
            .expect("first connect");

        // Install a second transport, which bumps the generation.
        let (t2, _dc2) = make_mock(false);
        let (_tx2, rx2) = tokio::sync::mpsc::channel(1);
        svc.connect_with_transport(t2 as Arc<dyn Transport>, rx2, Duration::ZERO)
            .await
            .expect("second connect");

        // Close T1's inbound channel. Its reader task will see EOF, then
        // observe that conn_generation != its generation and exit cleanly.
        drop(tx1);
        // Yield to let the reader task run.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // T2 must still be in the slot.
        assert!(
            svc.inner.transport.lock().await.is_some(),
            "T2 must still be in the slot after T1's stale reader exits"
        );
    }

    #[tokio::test]
    async fn disconnect_clears_reconnect_config() {
        let svc = make_service().await;
        let _g = keep_alive(&svc);

        let (t, _dc) = make_mock(false);
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        svc.connect_with_transport(t as Arc<dyn Transport>, rx, Duration::ZERO)
            .await
            .expect("connect");

        // Arm reconnect_config as connect_by_address/serial would.
        *svc.inner.reconnect_config.lock().await =
            Some(ReconnectConfig::Serial { path: "/dev/fake".into(), baud: 115200 });

        svc.disconnect().await.expect("disconnect");

        assert!(
            svc.inner.reconnect_config.lock().await.is_none(),
            "disconnect must clear reconnect_config"
        );
        assert!(
            svc.inner.transport.lock().await.is_none(),
            "disconnect must clear transport slot"
        );
    }
}

impl MeshtasticService {
    /// Maximum voice-frame body size (excluding the 16-byte chunk
    /// header) that the currently-attached transport can carry intact
    /// in a single outbound write. Falls back to [`MAX_BODY_SIZE`] when
    /// the transport reports no per-write cap (USB serial, loopback).
    ///
    /// Inherent rather than trait-only so the native
    /// [`crate::voice::sender::VoiceSender`] can size chunks without
    /// pulling [`crate::voice::sink::VoiceFrameSink`] into scope.
    pub fn max_voice_body_size(&self) -> usize {
        // ToRadio wrapping adds protobuf field tags + length varints +
        // MeshPacket headers (from/to/id/channel/portnum) on top of
        // the raw voice frame (16-byte header + body). Empirically
        // ~32-44 bytes depending on varint width; 48 is a safe upper
        // bound that leaves headroom across all realistic from/to/id
        // values without slicing into transports' usable payload.
        const TORADIO_OVERHEAD_BYTES: usize = 48;
        const HEADER_SIZE: usize = crate::voice::consts::HEADER_SIZE;
        const MAX_BODY_SIZE: usize = crate::voice::consts::MAX_BODY_SIZE;

        let tx_max = self.inner.transport_max_tx_payload.load(Ordering::Relaxed);
        tx_max
            .saturating_sub(TORADIO_OVERHEAD_BYTES)
            .saturating_sub(HEADER_SIZE)
            .min(MAX_BODY_SIZE)
    }
}

#[async_trait::async_trait]
impl crate::voice::sink::VoiceFrameSink for MeshtasticService {
    async fn enqueue_voice_frame_with_id(
        &self,
        frame: Vec<u8>,
        channel: u32,
        to: Option<NodeId>,
        want_ack: bool,
        pacing: Duration,
    ) -> Result<u32> {
        let to_u32 = to.map(|id| id.as_u32());
        self.enqueue_voice_frame_with_id(frame, channel, to_u32, want_ack, pacing)
            .await
    }

    fn subscribe_voice_data(&self) -> broadcast::Receiver<VoiceData> {
        self.inner.voice_data_tx.subscribe()
    }

    fn max_voice_body_size(&self) -> usize {
        MeshtasticService::max_voice_body_size(self)
    }
}
