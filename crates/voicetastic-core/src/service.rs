//! High-level façade over [`crate::ble`], [`crate::serial`], and [`crate::proto`].
//!
//! [`MeshService`] owns either a BLE [`crate::ble::Connection`] or a
//! [`crate::serial::SerialConnection`], sends a `WantConfigId` handshake on
//! connect, fans incoming `FromRadio` messages out to typed observers, and
//! exposes outbound helpers (`send_text`, `send_data`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use prost::Message as _;
use tokio::sync::{broadcast, mpsc, watch, Mutex};
use tracing::{debug, info, warn};

use crate::ble::{BleManager, Connection, DiscoveredDevice, CONFIG_REQUEST_DELAY};
use crate::error::{Error, Result};
use crate::ids::node_num_to_id;
use crate::ports::{BROADCAST_ADDR, PRIVATE_APP, TEXT_MESSAGE_APP};
use crate::proto::{
    from_radio, mesh_packet, to_radio, Data, FromRadio, MeshPacket, MyNodeInfo, NodeInfo, PortNum,
    ToRadio, User,
};
use crate::serial::SerialConnection;

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

/// Abstraction over BLE vs serial transport.
enum Transport {
    Ble(Arc<Connection>),
    Serial(Arc<SerialConnection>),
}

impl Transport {
    async fn write_to_radio(&self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Ble(c) => c.write_to_radio(bytes).await,
            Self::Serial(c) => c.write_to_radio(bytes).await,
        }
    }

    async fn disconnect(&self) -> Result<()> {
        match self {
            Self::Ble(c) => c.disconnect().await,
            Self::Serial(c) => c.disconnect().await,
        }
    }
}

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

        for p in conn.drain_from_radio().await? {
            let _ = self.handle_from_radio(&p).await;
        }

        tokio::time::sleep(CONFIG_REQUEST_DELAY).await;
        self.set_state(ConnectionState::Configuring);
        self.send_want_config().await?;
        Ok(())
    }

    /// Connect to a device by serial port path (e.g. `/dev/ttyUSB0`).
    pub async fn connect_by_serial(&self, path: &str) -> Result<()> {
        self.connect_by_serial_baud(path, crate::serial::DEFAULT_BAUD).await
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

    async fn send_want_config(&self) -> Result<()> {
        let nonce: u32 = rand_u32();
        info!(nonce, "sending want_config_id");
        self.send_to_radio(to_radio::PayloadVariant::WantConfigId(nonce))
            .await
    }

    /// Send a UTF-8 text message. `to` defaults to [`BROADCAST_ADDR`].
    pub async fn send_text(&self, text: &str, channel: u32, to: Option<u32>) -> Result<u32> {
        let id = self.next_id().await;
        let pkt = MeshPacket {
            from: 0,
            to: to.unwrap_or(BROADCAST_ADDR),
            channel,
            id,
            want_ack: true,
            hop_limit: 3,
            priority: mesh_packet::Priority::Default as i32,
            payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
                portnum: PortNum::TextMessageApp as i32,
                payload: text.as_bytes().to_vec(),
                ..Default::default()
            })),
            ..Default::default()
        };
        let _ = TEXT_MESSAGE_APP; // keep ports symbol used
        self.send_to_radio(to_radio::PayloadVariant::Packet(pkt))
            .await?;
        Ok(id)
    }

    /// Send a raw application data packet (e.g. voice chunks via [`PRIVATE_APP`]).
    pub async fn send_data(
        &self,
        portnum: i32,
        payload: Vec<u8>,
        channel: u32,
        to: Option<u32>,
        want_ack: bool,
    ) -> Result<u32> {
        let id = self.next_id().await;
        let pkt = MeshPacket {
            from: 0,
            to: to.unwrap_or(BROADCAST_ADDR),
            channel,
            id,
            want_ack,
            hop_limit: 3,
            priority: mesh_packet::Priority::Default as i32,
            payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
                portnum,
                payload,
                ..Default::default()
            })),
            ..Default::default()
        };
        self.send_to_radio(to_radio::PayloadVariant::Packet(pkt))
            .await?;
        Ok(id)
    }

    /// Send pre-chunked voice payloads, sleeping
    /// [`crate::voice::INTER_CHUNK_DELAY_MS`] between each.
    pub async fn send_voice_chunks(
        &self,
        chunks: Vec<Vec<u8>>,
        channel: u32,
        to: Option<u32>,
    ) -> Result<Vec<u32>> {
        let mut ids = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.into_iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(crate::voice::INTER_CHUNK_DELAY_MS))
                    .await;
            }
            ids.push(
                self.send_data(PRIVATE_APP as i32, chunk, channel, to, false)
                    .await?,
            );
        }
        Ok(ids)
    }

    async fn next_id(&self) -> u32 {
        let mut g = self.inner.next_packet_id.lock().await;
        let id = *g;
        *g = g.wrapping_add(1).max(1);
        id
    }

    async fn send_to_radio(&self, payload: to_radio::PayloadVariant) -> Result<()> {
        let transport = {
            let slot = self.inner.transport.lock().await;
            match slot.as_ref() {
                Some(Transport::Ble(c)) => Transport::Ble(c.clone()),
                Some(Transport::Serial(c)) => Transport::Serial(c.clone()),
                None => return Err(Error::NotConnected),
            }
        };
        self.send_to_radio_via(&transport, payload).await
    }

    async fn send_to_radio_via(
        &self,
        transport: &Transport,
        payload: to_radio::PayloadVariant,
    ) -> Result<()> {
        let msg = ToRadio {
            payload_variant: Some(payload),
        };
        let mut buf = Vec::with_capacity(msg.encoded_len());
        msg.encode(&mut buf)?;
        transport.write_to_radio(&buf).await
    }

    async fn handle_from_radio(&self, bytes: &[u8]) -> Result<()> {
        let msg = FromRadio::decode(bytes)?;
        let Some(variant) = msg.payload_variant else {
            return Ok(());
        };
        match variant {
            from_radio::PayloadVariant::MyInfo(info) => {
                debug!(my_node_num = info.my_node_num, "MyNodeInfo");
                let _ = self.inner.my_info_tx.send(Some(info));
            }
            from_radio::PayloadVariant::NodeInfo(ni) => {
                let mut nodes = self.inner.nodes_tx.borrow().clone();
                nodes.insert(ni.num, ni);
                let _ = self.inner.nodes_tx.send(nodes);
            }
            from_radio::PayloadVariant::ConfigCompleteId(nonce) => {
                info!(nonce, "config_complete");
                self.set_state(ConnectionState::Ready);
                let _ = self.inner.config_complete_tx.send(nonce);
            }
            from_radio::PayloadVariant::Packet(pkt) => {
                self.handle_packet(pkt);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_packet(&self, pkt: MeshPacket) {
        let Some(mesh_packet::PayloadVariant::Decoded(data)) = pkt.payload_variant.as_ref() else {
            return;
        };
        let portnum = data.portnum;
        let payload = data.payload.clone();
        if portnum == PortNum::TextMessageApp as i32 {
            if let Ok(text) = String::from_utf8(payload.clone()) {
                let from_id = node_num_to_id(pkt.from);
                let _ = self.inner.incoming_text_tx.send(IncomingText {
                    from: pkt.from,
                    from_id,
                    to: pkt.to,
                    channel: pkt.channel,
                    text,
                    rx_time: pkt.rx_time,
                    rx_snr: pkt.rx_snr,
                    rx_rssi: pkt.rx_rssi,
                });
                return;
            }
        }
        let _ = self.inner.incoming_data_tx.send(IncomingData {
            from: pkt.from,
            to: pkt.to,
            channel: pkt.channel,
            portnum,
            payload,
            rx_time: pkt.rx_time,
        });
    }

    fn set_state(&self, state: ConnectionState) {
        let _ = self.inner.state_tx.send(state);
    }
}

/// Long name accessor for callers that don't want to import `proto::User`.
pub fn node_long_name(node: &NodeInfo) -> Option<&str> {
    node.user.as_ref().map(|u: &User| u.long_name.as_str())
}

fn rand_u32() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos ^ 0x9E37_79B1
}
