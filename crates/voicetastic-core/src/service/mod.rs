//! High-level façade over [`crate::ble`], [`crate::serial`], and [`crate::proto`].
//!
//! [`MeshService`] owns either a BLE [`crate::ble::Connection`] or a
//! [`crate::serial::SerialConnection`], sends a `WantConfigId` handshake on
//! connect, fans incoming `FromRadio` messages out to typed observers, and
//! exposes outbound helpers (`send_text`, `send_data`).

mod inbound;
mod outbound;
mod transport;
mod types;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, broadcast, mpsc, watch};
use tracing::warn;

/// Maximum time we wait for the device to finish its config burst (i.e. for
/// `ConnectionState` to leave `Configuring`). If the radio never sends
/// `ConfigCompleteId`, we revert to `Connected` so the UI isn't stranded.
const CONFIG_BURST_TIMEOUT: Duration = Duration::from_secs(30);

use crate::ble::{BleManager, CONFIG_REQUEST_DELAY, Connection, DiscoveredDevice};
use crate::error::Result;
use crate::proto::{
    Channel, DeviceMetadata, MyNodeInfo, NodeInfo, User,
    config::{
        BluetoothConfig, DeviceConfig, DisplayConfig, LoRaConfig, NetworkConfig, PositionConfig,
        PowerConfig,
    },
    to_radio,
};
use crate::serial::SerialConnection;

use transport::Transport;

pub use types::{ConnectionState, IncomingData, IncomingText, node_long_name};

/// Service handle. Cheap to clone — internally `Arc`'d.
#[derive(Clone)]
pub struct MeshService {
    inner: Arc<Inner>,
}

struct Inner {
    ble: tokio::sync::OnceCell<BleManager>,
    transport: Mutex<Option<Transport>>,
    state_tx: watch::Sender<ConnectionState>,
    my_info_tx: watch::Sender<Option<MyNodeInfo>>,
    nodes_tx: watch::Sender<HashMap<u32, NodeInfo>>,
    config_complete_tx: broadcast::Sender<u32>,
    incoming_text_tx: broadcast::Sender<IncomingText>,
    incoming_data_tx: broadcast::Sender<IncomingData>,
    next_packet_id: Mutex<u32>,
    // Configuration sections, each updated when the device emits its
    // matching `Config` chunk during the want-config burst.
    pub(super) lora_tx: watch::Sender<Option<LoRaConfig>>,
    pub(super) device_tx: watch::Sender<Option<DeviceConfig>>,
    pub(super) position_tx: watch::Sender<Option<PositionConfig>>,
    pub(super) power_tx: watch::Sender<Option<PowerConfig>>,
    pub(super) network_tx: watch::Sender<Option<NetworkConfig>>,
    pub(super) display_tx: watch::Sender<Option<DisplayConfig>>,
    pub(super) bluetooth_tx: watch::Sender<Option<BluetoothConfig>>,
    pub(super) channels_tx: watch::Sender<Vec<Channel>>,
    pub(super) owner_tx: watch::Sender<Option<User>>,
    pub(super) metadata_tx: watch::Sender<Option<DeviceMetadata>>,
}

impl MeshService {
    pub async fn new() -> Result<Self> {
        let ble = tokio::sync::OnceCell::new();
        let (state_tx, _) = watch::channel(ConnectionState::Disconnected);
        let (my_info_tx, _) = watch::channel(None);
        let (nodes_tx, _) = watch::channel(HashMap::new());
        let (config_complete_tx, _) = broadcast::channel(8);
        let (incoming_text_tx, _) = broadcast::channel(64);
        let (incoming_data_tx, _) = broadcast::channel(128);
        let (lora_tx, _) = watch::channel(None);
        let (device_tx, _) = watch::channel(None);
        let (position_tx, _) = watch::channel(None);
        let (power_tx, _) = watch::channel(None);
        let (network_tx, _) = watch::channel(None);
        let (display_tx, _) = watch::channel(None);
        let (bluetooth_tx, _) = watch::channel(None);
        let (channels_tx, _) = watch::channel(Vec::new());
        let (owner_tx, _) = watch::channel(None);
        let (metadata_tx, _) = watch::channel(None);
        Ok(Self {
            inner: Arc::new(Inner {
                ble,
                transport: Mutex::new(None),
                state_tx,
                my_info_tx,
                nodes_tx,
                config_complete_tx,
                incoming_text_tx,
                incoming_data_tx,
                next_packet_id: Mutex::new(1),
                lora_tx,
                device_tx,
                position_tx,
                power_tx,
                network_tx,
                display_tx,
                bluetooth_tx,
                channels_tx,
                owner_tx,
                metadata_tx,
            }),
        })
    }

    pub async fn scan(&self) -> Result<mpsc::Receiver<DiscoveredDevice>> {
        self.ble().await?.scan().await
    }
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
            .my_info_tx
            .borrow()
            .as_ref()
            .map(|i| i.my_node_num)
    }

    /// Re-request the entire configuration burst.
    pub async fn refresh_config(&self) -> Result<()> {
        // Clear local snapshots so callers can detect a fresh burst.
        let _ = self.inner.lora_tx.send(None);
        let _ = self.inner.device_tx.send(None);
        let _ = self.inner.position_tx.send(None);
        let _ = self.inner.power_tx.send(None);
        let _ = self.inner.network_tx.send(None);
        let _ = self.inner.display_tx.send(None);
        let _ = self.inner.bluetooth_tx.send(None);
        let _ = self.inner.channels_tx.send(Vec::new());
        let prev = *self.inner.state_tx.borrow();
        self.set_state(ConnectionState::Configuring);
        if let Err(e) = self.send_want_config().await {
            // Don't strand the UI in `Configuring` if we can't actually ask.
            self.set_state(prev);
            return Err(e);
        }
        self.spawn_config_watchdog();
        Ok(())
    }

    /// Connect to a peripheral by BLE address (`AA:BB:CC:DD:EE:FF`).
    pub async fn connect_by_address(&self, address: &str) -> Result<()> {
        self.set_state(ConnectionState::Connecting);
        let peripheral = self.ble().await?.peripheral_by_address(address).await?;
        let conn = Arc::new(Connection::open(peripheral).await?);
        let transport = Transport::Ble(conn.clone());
        {
            let mut slot = self.inner.transport.lock().await;
            *slot = Some(transport);
        }
        self.set_state(ConnectionState::Connected);

        // `subscribe_inbound` already drains on every notify and on the safety
        // poll, so we must NOT drain here too: btleplug serialises GATT reads
        // per peripheral, but issuing concurrent drains would still race for
        // the FROMRADIO queue ordering.
        let mut inbound = conn.clone().subscribe_inbound().await?;
        let svc = self.clone();
        tokio::spawn(async move {
            while let Some(payload) = inbound.recv().await {
                if let Err(e) = svc.handle_from_radio(&payload).await {
                    warn!(?e, "from_radio handler failed");
                }
            }
            svc.set_state(ConnectionState::Disconnected);
            let mut slot = svc.inner.transport.lock().await;
            *slot = None;
        });

        tokio::time::sleep(CONFIG_REQUEST_DELAY).await;
        self.set_state(ConnectionState::Configuring);
        self.send_want_config().await?;
        self.spawn_config_watchdog();
        Ok(())
    }

    /// Connect to a device by serial port path (e.g. `/dev/ttyUSB0`).
    pub async fn connect_by_serial(&self, path: &str) -> Result<()> {
        self.connect_by_serial_baud(path, crate::serial::DEFAULT_BAUD)
            .await
    }

    /// Connect to a device by serial port path with a custom baud rate.
    pub async fn connect_by_serial_baud(&self, path: &str, baud: u32) -> Result<()> {
        self.set_state(ConnectionState::Connecting);
        let serial = Arc::new(SerialConnection::open(path, baud).await?);
        let transport = Transport::Serial(serial.clone());
        {
            let mut slot = self.inner.transport.lock().await;
            *slot = Some(transport);
        }
        self.set_state(ConnectionState::Connected);

        let mut inbound = serial.subscribe_inbound().await?;
        let svc = self.clone();
        tokio::spawn(async move {
            while let Some(payload) = inbound.recv().await {
                if let Err(e) = svc.handle_from_radio(&payload).await {
                    warn!(?e, "from_radio handler failed");
                }
            }
            svc.set_state(ConnectionState::Disconnected);
            let mut slot = svc.inner.transport.lock().await;
            *slot = None;
        });

        tokio::time::sleep(CONFIG_REQUEST_DELAY).await;
        self.set_state(ConnectionState::Configuring);
        self.send_want_config().await?;
        self.spawn_config_watchdog();
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<()> {
        let transport = {
            let mut slot = self.inner.transport.lock().await;
            slot.take()
        };
        if let Some(t) = transport {
            let _ = self
                .send_to_radio_via(&t, to_radio::PayloadVariant::Disconnect(true))
                .await;
            t.disconnect().await?;
        }
        self.set_state(ConnectionState::Disconnected);
        Ok(())
    }

    pub(super) fn set_state(&self, state: ConnectionState) {
        let _ = self.inner.state_tx.send(state);
    }

    /// Lazy-init the BLE adapter on first use. Lets `MeshService::new()`
    /// succeed on machines without Bluetooth (serial-only setups, CI hosts,
    /// integration tests). Failures still surface to the caller of any
    /// BLE-touching method.
    async fn ble(&self) -> Result<&BleManager> {
        self.inner
            .ble
            .get_or_try_init(|| async { BleManager::new().await })
            .await
    }

    /// Spawn a one-shot task that reverts `Configuring` to `Connected` if the
    /// device never sends `ConfigCompleteId` within [`CONFIG_BURST_TIMEOUT`].
    /// Cheap to call repeatedly; tasks self-exit if state has already moved on.
    fn spawn_config_watchdog(&self) {
        let svc = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(CONFIG_BURST_TIMEOUT).await;
            if *svc.inner.state_tx.borrow() == ConnectionState::Configuring {
                warn!(
                    timeout_s = CONFIG_BURST_TIMEOUT.as_secs(),
                    "config burst did not complete; reverting to Connected"
                );
                svc.set_state(ConnectionState::Connected);
            }
        });
    }
}

/// Compile-time assertion that `MeshService` can be cloned across `tokio::spawn`
/// boundaries and shared between threads. Catches refactors that accidentally
/// embed a `!Send` or `!Sync` field (e.g. `Rc`, `RefCell`, raw pointers).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync + Clone + 'static>() {}
    assert_send_sync::<MeshService>();
};

#[cfg(test)]
mod tests {
    //! Service-level tests that don't need real BLE or serial hardware.
    //!
    //! `MeshService::new()` lazy-inits the BLE adapter, so these tests can
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

    async fn make_service() -> MeshService {
        MeshService::new().await.expect("MeshService::new")
    }

    /// Hold one receiver on every watch we inspect: tokio's `watch::Sender::send`
    /// returns `Err` (without updating the cached value) when all receivers
    /// have been dropped. In production the GUI/CLI is always subscribed; in
    /// tests we have to keep the receivers alive ourselves.
    fn keep_alive(svc: &MeshService) -> Vec<Box<dyn std::any::Any + Send>> {
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

        // No transport: send_want_config will fail and refresh_config returns
        // Err — but the snapshots must already be cleared by then.
        let _ = svc.refresh_config().await;
        assert!(svc.inner.lora_tx.borrow().is_none());
        assert!(svc.inner.channels_tx.borrow().is_empty());
    }
}
