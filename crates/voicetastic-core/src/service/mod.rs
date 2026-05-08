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

use tokio::sync::{Mutex, broadcast, mpsc, watch};
use tracing::warn;

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
    ble: BleManager,
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
        let ble = BleManager::new().await?;
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
        self.inner.ble.scan().await
    }
    pub async fn stop_scan(&self) -> Result<()> {
        self.inner.ble.stop_scan().await
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
        self.set_state(ConnectionState::Configuring);
        self.send_want_config().await
    }

    /// Connect to a peripheral by BLE address (`AA:BB:CC:DD:EE:FF`).
    pub async fn connect_by_address(&self, address: &str) -> Result<()> {
        self.set_state(ConnectionState::Connecting);
        let peripheral = self.inner.ble.peripheral_by_address(address).await?;
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
}
