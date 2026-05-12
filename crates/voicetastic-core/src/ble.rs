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
pub const FROMNUM_UUID: Uuid = Uuid::from_u128(0xed9da18c_a800_4f66_a670_aa7547e34453);

/// Safety-net poll interval for `FROMRADIO` in case a notify is missed/coalesced.
pub const POLL_INTERVAL: Duration = Duration::from_millis(5_000);
/// Per-write timeout. Generous enough to absorb the first post-bond
/// write (on some firmwares the radio takes >2 s to ACK while finalising
/// its BLE security state machine) but still bounded so a wedged radio is
/// detected promptly.
pub const WRITE_TIMEOUT: Duration = Duration::from_millis(5_000);
/// Delay between GATT setup completion and the first `want_config_id` request.
pub const CONFIG_REQUEST_DELAY: Duration = Duration::from_millis(300);
/// How many times to retry `discover_services` on transient failures.
pub const SERVICE_DISCOVERY_RETRIES: usize = 5;
/// Per-attempt timeout for service discovery. BlueZ-side default is ~30 s
/// which is far too long for an interactive UI; bound each attempt so we can
/// retry while ServicesResolved is still pending.
pub const SERVICE_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
/// Delay after `connect` before issuing `discover_services`. BlueZ needs a
/// brief window to flip `ServicesResolved` after the link comes up.
pub const POST_CONNECT_DELAY: Duration = Duration::from_millis(500);

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
    /// Linux-only in-process BlueZ pairing agent. When present, the
    /// `prepare_link` path drives pairing over DBus on the same
    /// connection the agent is registered on, so passkey prompts come
    /// back to us instead of to the desktop bluetooth applet.
    #[cfg(target_os = "linux")]
    pairing: Option<std::sync::Arc<crate::pairing::PairingAgent>>,
    /// Receiver side of the pairing-prompt channel, handed out exactly
    /// once via [`Self::take_pairing_prompts`].
    #[cfg(target_os = "linux")]
    pairing_rx: tokio::sync::Mutex<Option<mpsc::Receiver<crate::pairing::PairingPrompt>>>,
}

impl BleManager {
    pub async fn new() -> Result<Self> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        let adapter = adapters
            .into_iter()
            .next()
            .ok_or_else(|| Error::Other("no BLE adapter found".into()))?;

        #[cfg(target_os = "linux")]
        let (pairing, pairing_rx) = match crate::pairing::PairingAgent::install().await {
            Ok((agent, rx)) => (Some(std::sync::Arc::new(agent)), Some(rx)),
            Err(e) => {
                warn!(error = %e, "failed to register BlueZ pairing agent; PIN prompts will not be routed to the app");
                (None, None)
            }
        };

        Ok(Self {
            adapter,
            #[cfg(target_os = "linux")]
            pairing,
            #[cfg(target_os = "linux")]
            pairing_rx: tokio::sync::Mutex::new(pairing_rx),
        })
    }

    /// Take ownership of the pairing-prompt receiver. Returns `None` if
    /// the agent failed to register or if it has already been taken.
    #[cfg(target_os = "linux")]
    pub async fn take_pairing_prompts(
        &self,
    ) -> Option<mpsc::Receiver<crate::pairing::PairingPrompt>> {
        self.pairing_rx.lock().await.take()
    }

    /// Stream Meshtastic peripherals visible to the host.
    ///
    /// We do two things in parallel:
    ///
    /// 1. Pre-populate from BlueZ's local cache — every peripheral the
    ///    daemon already knows about (paired, previously connected, or
    ///    just seen during a prior scan) that advertises
    ///    [`SERVICE_UUID`]. This is what gives the user instant results
    ///    for radios they've used before, with no scan latency.
    /// 2. Start an active LE scan filtered on [`SERVICE_UUID`] and
    ///    forward `DeviceDiscovered` / `DeviceUpdated` events for any
    ///    newly-advertising radio. The scan keeps running until the
    ///    caller invokes [`Self::stop_scan`] (or the receiver is
    ///    dropped, which also tears the task down).
    ///
    /// Each peripheral is only reported once per call. The scan
    /// deliberately includes unpaired devices — pairing is handled
    /// downstream by [`Self::prepare_link`] / the GUI passkey modal.
    pub async fn scan(&self) -> Result<mpsc::Receiver<DiscoveredDevice>> {
        let (tx, rx) = mpsc::channel(32);
        let mut seen: std::collections::HashSet<PeripheralId> = std::collections::HashSet::new();

        // 1. Seed from BlueZ's local cache.
        for p in self.adapter.peripherals().await? {
            let props = match p.properties().await {
                Ok(Some(props)) => props,
                _ => continue,
            };
            if !props.services.contains(&SERVICE_UUID) {
                continue;
            }
            let id = p.id();
            if !seen.insert(id.clone()) {
                continue;
            }
            let dev = DiscoveredDevice {
                id,
                name: props.local_name.clone(),
                address: p.address().to_string(),
            };
            debug!(address = %dev.address, name = ?dev.name, "cached Meshtastic peripheral");
            if tx.send(dev).await.is_err() {
                return Ok(rx);
            }
        }

        // 2. Start an active scan and forward new discoveries on a
        //    background task. The task exits when the receiver is
        //    dropped or `stop_scan` closes the events stream.
        let filter = ScanFilter {
            services: vec![SERVICE_UUID],
        };
        if let Err(e) = self.adapter.start_scan(filter).await {
            warn!(error = %e, "active scan failed to start; returning cached results only");
            return Ok(rx);
        }
        let events = self.adapter.events().await?;
        let adapter = self.adapter.clone();
        tokio::spawn(async move {
            let mut events = events;
            while let Some(ev) = events.next().await {
                let id = match ev {
                    CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) => id,
                    _ => continue,
                };
                if !seen.insert(id.clone()) {
                    continue;
                }
                let Ok(p) = adapter.peripheral(&id).await else {
                    continue;
                };
                let props = p.properties().await.ok().flatten();
                // The scan filter is best-effort on some BlueZ versions;
                // double-check the service UUID is actually advertised.
                let has_service = props
                    .as_ref()
                    .map(|p| p.services.contains(&SERVICE_UUID))
                    .unwrap_or(false);
                if !has_service {
                    continue;
                }
                let name = props.as_ref().and_then(|p| p.local_name.clone());
                let address = p.address().to_string();
                let dev = DiscoveredDevice { id, name, address };
                debug!(address = %dev.address, name = ?dev.name, "advertised Meshtastic peripheral");
                if tx.send(dev).await.is_err() {
                    break;
                }
            }
        });
        Ok(rx)
    }

    /// Stop the active LE scan started by [`Self::scan`]. Safe to call
    /// when no scan is running.
    pub async fn stop_scan(&self) -> Result<()> {
        let _ = self.adapter.stop_scan().await;
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

    /// Active-scan for advertising Meshtastic devices, including those that
    /// are **not** currently paired with this host.
    ///
    /// Unlike [`Self::scan`] this temporarily issues `Adapter::start_scan`
    /// for `timeout` so we can discover peripherals before any system-level
    /// connection exists. Used by the in-app pairing flow only; everyday
    /// connect-to-known-device traffic should keep using [`Self::scan`].
    ///
    /// Returns one entry per peripheral that advertises [`SERVICE_UUID`]
    /// during the window. Stops the scan before returning.
    pub async fn discover_pairable(&self, timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
        let filter = ScanFilter {
            services: vec![SERVICE_UUID],
        };
        self.adapter.start_scan(filter).await?;
        let mut events = self.adapter.events().await?;
        let adapter = self.adapter.clone();
        let mut seen: std::collections::HashMap<PeripheralId, DiscoveredDevice> =
            std::collections::HashMap::new();

        let collect = async {
            while let Some(ev) = events.next().await {
                if let CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) = ev
                    && let Ok(p) = adapter.peripheral(&id).await
                {
                    let props = p.properties().await.ok().flatten();
                    // Active-scan filter is best-effort on some bluez versions;
                    // double-check the service UUID is actually advertised.
                    let has_service = props
                        .as_ref()
                        .map(|p| p.services.contains(&SERVICE_UUID))
                        .unwrap_or(false);
                    if !has_service {
                        continue;
                    }
                    let name = props.as_ref().and_then(|p| p.local_name.clone());
                    let address = p.address().to_string();
                    seen.insert(id.clone(), DiscoveredDevice { id, name, address });
                }
            }
        };

        let _ = tokio::time::timeout(timeout, collect).await;
        let _ = self.adapter.stop_scan().await;
        Ok(seen.into_values().collect())
    }

    /// Pair, trust and connect a Meshtastic device.
    ///
    /// On Linux this drives BlueZ directly over DBus, using the same
    /// connection our [`PairingAgent`](crate::pairing::PairingAgent) is
    /// registered on so any passkey prompts the radio raises are
    /// delivered to subscribers of [`Self::take_pairing_prompts`]
    /// (typically the GUI's modal dialog). On platforms without an
    /// in-process agent we never reach this path — the host BLE stack
    /// handles pairing through its own UI.
    ///
    /// Idempotent on `AlreadyExists` / `AlreadyConnected`. Auto-recovers
    /// from a stale bond by removing the device and retrying once.
    #[cfg(target_os = "linux")]
    pub async fn prepare_link(&self, address: &str) -> Result<()> {
        let agent = self
            .pairing
            .as_ref()
            .ok_or_else(|| Error::Other("BlueZ pairing agent not registered".into()))?;
        crate::pairing::pair_and_connect(agent.connection(), address).await
    }

    /// No-op fallback on non-Linux platforms: the system BLE stack handles
    /// pairing through its own UI. Returns `NotConnected` so the caller
    /// surfaces a useful error if the user hasn't paired the device
    /// elsewhere.
    #[cfg(not(target_os = "linux"))]
    pub async fn prepare_link(&self, _address: &str) -> Result<()> {
        Err(Error::NotConnected)
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
    /// Attach to an **already-connected** peripheral: validate the link is
    /// up, discover services (with retry), enable `FROMNUM` notifies, and
    /// resolve the three characteristics we need.
    ///
    /// We never call `peripheral.connect()` — pairing, bonding and the
    /// initial LE connection are the operating system's job (e.g.
    /// `bluetoothctl connect …` or the desktop's Bluetooth settings). If
    /// the peripheral is not currently connected we bail out with
    /// [`Error::NotConnected`] instead of opening a fresh link, so the
    /// user is prompted to connect via the system UI and we don't
    /// accidentally re-pair or fight the OS bond manager.
    pub async fn open(peripheral: Peripheral) -> Result<Self> {
        if !peripheral.is_connected().await? {
            return Err(Error::NotConnected);
        }
        // Give BlueZ a moment to populate the GATT DB before discover_services.
        // ServicesResolved may still be settling even on an already-up link
        // (e.g. immediately after a system-side reconnect).
        tokio::time::sleep(POST_CONNECT_DELAY).await;

        let mut last_err = None;
        for attempt in 1..=SERVICE_DISCOVERY_RETRIES {
            match peripheral
                .discover_services_with_timeout(SERVICE_DISCOVERY_TIMEOUT)
                .await
            {
                Ok(_) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    warn!(?attempt, ?e, "discover_services failed, retrying");
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_millis(500)).await;
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
        let addr = peripheral.address().to_string();

        // Drain any FROMRADIO bytes left over from a previous phone-API
        // session before the upper layer issues WantConfigId. Real
        // Meshtastic firmware will tear the link down with ATT "Unlikely
        // Error" (0x0e) if a new WantConfigId arrives while stale frames
        // are still queued — the Android app and meshtastic-python do
        // the same pre-drain. Reads happen *after* subscribe so a notify
        // can wake us for any frame that arrives mid-drain. We bound the
        // drain to keep startup snappy if the radio is wedged.
        {
            let mut drained = 0usize;
            for _ in 0..32 {
                match peripheral.read(&from_radio).await {
                    Ok(p) if p.is_empty() => break,
                    Ok(p) => {
                        drained += p.len();
                    }
                    Err(e) => {
                        warn!(?e, "initial FROMRADIO drain read failed");
                        break;
                    }
                }
            }
            if drained > 0 {
                debug!(bytes = drained, "drained stale FROMRADIO bytes");
            }
        }

        info!(address = %addr, "Meshtastic GATT setup complete");

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
    ///
    /// The `TORADIO` characteristic on real Meshtastic firmware advertises
    /// only the plain `write` flag (= `WriteWithResponse`) — *not*
    /// `write-without-response`. Using `WithoutResponse` against it makes
    /// BlueZ accept the call locally but never deliver an ATT op the radio
    /// recognises, so the radio's "client never sent want_config_id"
    /// inactivity timer fires a couple of seconds later and tears the link
    /// down. Pick the `WriteType` from the characteristic's declared
    /// properties so we match whichever flag the firmware exposes.
    pub async fn write_to_radio(&self, bytes: &[u8]) -> Result<()> {
        let _permit = self
            .write_sema
            .acquire()
            .await
            .map_err(|_| Error::Other("write semaphore closed".into()))?;
        let _g = self.write_lock.lock().await;
        let kind = if self
            .to_radio
            .properties
            .contains(btleplug::api::CharPropFlags::WRITE)
        {
            WriteType::WithResponse
        } else {
            WriteType::WithoutResponse
        };
        debug!(addr = %self.peripheral.address(), len = bytes.len(), ?kind, "TORADIO write begin");
        let res = timeout(
            WRITE_TIMEOUT,
            self.peripheral.write(&self.to_radio, bytes, kind),
        )
        .await;
        match res {
            Ok(Ok(())) => {
                debug!("TORADIO write ok");
                Ok(())
            }
            Ok(Err(e)) => {
                warn!(?e, "TORADIO write failed");
                Err(e.into())
            }
            Err(_) => {
                warn!("TORADIO write timed out");
                Err(Error::WriteTimeout)
            }
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
            // `tokio::time::interval` fires the first tick *immediately*.
            // We must not poll `FROMRADIO` before the upper layer has had a
            // chance to send `WantConfigId`: on real firmware the phone-API
            // session is closed until the radio receives that frame, and a
            // pre-session read is answered with ATT 0x0e ("Unlikely Error")
            // and a link tear-down. Consume the immediate tick so the first
            // real poll happens one POLL_INTERVAL after subscribe_inbound
            // returns — by which point the service layer has issued its
            // initial TORADIO write.
            interval.tick().await;
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

    /// Detach from the peripheral.
    ///
    /// We only signal our background tasks to stop and unsubscribe from
    /// notifications — the underlying LE link is owned by the operating
    /// system (the user connected the device via `bluetoothctl` / the
    /// desktop's Bluetooth panel) so we deliberately do **not** call
    /// `peripheral.disconnect()`. Tearing the link down here would surprise
    /// other apps using the same radio and force the user to reconnect
    /// from the OS UI before the next session.
    pub async fn disconnect(&self) -> Result<()> {
        let _ = self.shutdown.send(true);
        // Best-effort unsubscribe; ignore errors (already gone, etc.).
        let _ = self
            .peripheral
            .unsubscribe(&self.from_radio_notify_char())
            .await;
        Ok(())
    }

    /// FROMNUM characteristic handle, looked up on demand so we don't have
    /// to store it on the struct just for unsubscribe.
    #[allow(clippy::wrong_self_convention)]
    fn from_radio_notify_char(&self) -> btleplug::api::Characteristic {
        // Cheap: peripheral.characteristics() returns from the cached GATT
        // DB populated during open().
        self.peripheral
            .characteristics()
            .into_iter()
            .find(|c| c.uuid == FROMNUM_UUID)
            .unwrap_or_else(|| self.from_radio.clone())
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
