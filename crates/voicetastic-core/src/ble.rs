//! BLE GATT transport for Meshtastic nodes.
//!
//! Implementation uses [`btleplug`] which talks to BlueZ over DBus on Linux.

use std::sync::Arc;
use std::time::Duration;

use btleplug::api::{Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures::stream::StreamExt;
use tokio::sync::{Mutex, Semaphore, mpsc, watch};
use tokio::time::timeout;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::error::{Error, Result};

/// Meshtastic BLE service UUID.
pub const SERVICE_UUID: Uuid = Uuid::from_u128(0x6ba1b218_15a8_461f_9fa8_5dcae273eafd);
/// `TORADIO` characteristic (Write).
pub const TORADIO_UUID: Uuid = Uuid::from_u128(0xf75c76d2_129e_4dad_a1dd_7866124401e7);
/// `FROMRADIO` characteristic (Read).
pub const FROMRADIO_UUID: Uuid = Uuid::from_u128(0x2c55e69e_4993_11ed_b878_0242ac120002);
/// `FROMNUM` characteristic (Notify).
pub const FROMNUM_UUID: Uuid = Uuid::from_u128(0xed9da18c_a800_4f66_a670_aa7547de15e6);

/// Safety-net poll interval for `FROMRADIO` in case a notify is missed/coalesced.
pub const POLL_INTERVAL: Duration = Duration::from_millis(5_000);
/// Per-write timeout (matches Kotlin's 2 s GATT write semaphore guard).
pub const WRITE_TIMEOUT: Duration = Duration::from_millis(2_000);
/// Delay between GATT setup completion and the first `want_config_id` request.
pub const CONFIG_REQUEST_DELAY: Duration = Duration::from_millis(300);
/// How many times to retry `discover_services` on transient failures.
pub const SERVICE_DISCOVERY_RETRIES: usize = 3;

/// A peripheral discovered during scanning.
#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub id: PeripheralId,
    pub name: Option<String>,
    pub address: String,
}

/// Top-level BLE manager wrapper.
pub struct BleManager {
    adapter: Adapter,
}

impl BleManager {
    pub async fn new() -> Result<Self> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        let adapter = adapters
            .into_iter()
            .next()
            .ok_or_else(|| Error::Other("no BLE adapter found".into()))?;
        Ok(Self { adapter })
    }

    /// Begin scanning for Meshtastic peripherals. Yields each device once
    /// discovered. The scan continues until the returned receiver is dropped.
    pub async fn scan(&self) -> Result<mpsc::Receiver<DiscoveredDevice>> {
        let filter = ScanFilter {
            services: vec![SERVICE_UUID],
        };
        self.adapter.start_scan(filter).await?;
        let mut events = self.adapter.events().await?;
        let adapter = self.adapter.clone();
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            while let Some(ev) = events.next().await {
                if let CentralEvent::DeviceDiscovered(id) = ev
                    && let Ok(p) = adapter.peripheral(&id).await
                {
                    let props = p.properties().await.ok().flatten();
                    let name = props.as_ref().and_then(|p| p.local_name.clone());
                    let address = p.address().to_string();
                    let dev = DiscoveredDevice {
                        id: id.clone(),
                        name,
                        address,
                    };
                    if tx.send(dev).await.is_err() {
                        break;
                    }
                }
            }
        });
        Ok(rx)
    }

    /// Stop the active scan.
    pub async fn stop_scan(&self) -> Result<()> {
        self.adapter.stop_scan().await?;
        Ok(())
    }

    /// Locate a peripheral by id.
    pub async fn peripheral(&self, id: &PeripheralId) -> Result<Peripheral> {
        Ok(self.adapter.peripheral(id).await?)
    }

    /// Locate a peripheral by address (case-insensitive).
    pub async fn peripheral_by_address(&self, addr: &str) -> Result<Peripheral> {
        let target = addr.to_ascii_lowercase();
        for p in self.adapter.peripherals().await? {
            if p.address().to_string().to_ascii_lowercase() == target {
                return Ok(p);
            }
        }
        Err(Error::Other(format!("no peripheral with address {addr}")))
    }
}

/// An open Meshtastic GATT connection.
///
/// Wraps a connected [`Peripheral`] plus the three characteristics we need.
/// All `to_radio` writes are serialised by an internal [`Semaphore`] and
/// guarded by [`WRITE_TIMEOUT`] to match the Kotlin write-gate that prevents
/// `GATT_WRITE_REQUEST_BUSY`.
pub struct Connection {
    pub peripheral: Peripheral,
    to_radio: btleplug::api::Characteristic,
    from_radio: btleplug::api::Characteristic,
    write_lock: Arc<Mutex<()>>,
    write_sema: Arc<Semaphore>,
    read_lock: Arc<Mutex<()>>,
    shutdown: watch::Sender<bool>,
}

impl Connection {
    /// Connect, discover services (with retry), enable `FROMNUM` notifies, and
    /// resolve the three characteristics we need.
    pub async fn open(peripheral: Peripheral) -> Result<Self> {
        if !peripheral.is_connected().await? {
            peripheral.connect().await?;
        }

        let mut last_err = None;
        for attempt in 1..=SERVICE_DISCOVERY_RETRIES {
            match peripheral.discover_services().await {
                Ok(_) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    warn!(?attempt, ?e, "discover_services failed, retrying");
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
        if let Some(e) = last_err {
            return Err(e.into());
        }

        let chars = peripheral.characteristics();
        let to_radio = chars
            .iter()
            .find(|c| c.uuid == TORADIO_UUID)
            .cloned()
            .ok_or(Error::MissingCharacteristic("TORADIO"))?;
        let from_radio = chars
            .iter()
            .find(|c| c.uuid == FROMRADIO_UUID)
            .cloned()
            .ok_or(Error::MissingCharacteristic("FROMRADIO"))?;
        let from_num = chars
            .iter()
            .find(|c| c.uuid == FROMNUM_UUID)
            .cloned()
            .ok_or(Error::MissingCharacteristic("FROMNUM"))?;

        peripheral.subscribe(&from_num).await?;
        info!("Meshtastic GATT setup complete");

        Ok(Self {
            peripheral,
            to_radio,
            from_radio,
            write_lock: Arc::new(Mutex::new(())),
            write_sema: Arc::new(Semaphore::new(1)),
            read_lock: Arc::new(Mutex::new(())),
            shutdown: watch::channel(false).0,
        })
    }

    /// Serialised, time-bounded write of a `ToRadio` payload.
    pub async fn write_to_radio(&self, bytes: &[u8]) -> Result<()> {
        let _permit = self
            .write_sema
            .acquire()
            .await
            .map_err(|_| Error::Other("write semaphore closed".into()))?;
        let _g = self.write_lock.lock().await;
        match timeout(
            WRITE_TIMEOUT,
            self.peripheral
                .write(&self.to_radio, bytes, WriteType::WithoutResponse),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(Error::WriteTimeout),
        }
    }

    /// Drain `FROMRADIO` until empty. Returns each non-empty payload in order.
    ///
    /// Read calls are serialised via `read_lock` so concurrent drains (notify
    /// task + safety-net poll) cannot interleave frames out of order.
    pub async fn drain_from_radio(&self) -> Result<Vec<Vec<u8>>> {
        let _g = self.read_lock.lock().await;
        let mut out = Vec::new();
        loop {
            let payload = self.peripheral.read(&self.from_radio).await?;
            if payload.is_empty() {
                break;
            }
            debug!(len = payload.len(), "FROMRADIO payload");
            out.push(payload);
        }
        Ok(out)
    }

    /// Subscribe to inbound `FROMRADIO` payloads.
    ///
    /// Spawns a background task that drains on every `FROMNUM` notify and
    /// re-polls every [`POLL_INTERVAL`] as a safety net. Both spawned tasks
    /// observe the [`Connection::disconnect`] signal and exit promptly.
    pub async fn subscribe_inbound(self: Arc<Self>) -> Result<mpsc::Receiver<Vec<u8>>> {
        let (tx, rx) = mpsc::channel(64);
        let conn = self.clone();
        let mut notifs = conn.peripheral.notifications().await?;
        let tx_notify = tx.clone();
        let conn_notify = conn.clone();
        let mut shutdown_notify = conn.shutdown.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_notify.changed() => return,
                    n = notifs.next() => {
                        let Some(n) = n else { return };
                        if n.uuid == FROMNUM_UUID
                            && let Ok(payloads) = conn_notify.drain_from_radio().await
                        {
                            for p in payloads {
                                if tx_notify.send(p).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        });
        let conn_poll = conn;
        let mut shutdown_poll = conn_poll.shutdown.subscribe();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_poll.changed() => return,
                    _ = interval.tick() => {
                        match conn_poll.drain_from_radio().await {
                            Ok(payloads) => {
                                for p in payloads {
                                    if tx.send(p).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(?e, "FROMRADIO poll failed");
                                return;
                            }
                        }
                    }
                }
            }
        });
        Ok(rx)
    }

    pub async fn disconnect(&self) -> Result<()> {
        let _ = self.shutdown.send(true);
        self.peripheral.disconnect().await?;
        Ok(())
    }
}

// Wire `Connection` into the cross-transport `MeshService` plumbing. The
// inherent methods above are kept (and remain the canonical API for direct
// BLE callers); this `impl` is a zero-cost forwarding shim so a
// `Arc<Connection>` can be stored as `Arc<dyn crate::Transport>`.
#[async_trait::async_trait]
impl crate::Transport for Connection {
    async fn write_to_radio(&self, bytes: &[u8]) -> Result<()> {
        Connection::write_to_radio(self, bytes).await
    }
    async fn disconnect(&self) -> Result<()> {
        Connection::disconnect(self).await
    }
}
