//! BlueZ pairing-agent integration.
//!
//! Meshtastic radios advertise a 6-digit BLE passkey on their OLED screen
//! during pairing (firmware default `123456`, user-configurable). On Linux
//! that passkey is delivered to whichever process is registered as the
//! current `org.bluez.Agent1` over DBus — typically the desktop's bluetooth
//! applet. When the user is interacting with this app, we want to take
//! over and route the prompt to our own GUI dialog instead, so the user
//! doesn't have to context-switch.
//!
//! This module is Linux-only. On other platforms the host BLE stack owns
//! the pairing UI and we never reach this code.
//!
//! ### Lifecycle
//!
//! 1. [`PairingAgent::install`] connects to the system bus, exports an
//!    `org.bluez.Agent1` at `/com/voicetastic/agent`, calls
//!    `AgentManager1.RegisterAgent(path, "KeyboardDisplay")` and
//!    `RequestDefaultAgent(path)`. The agent stays registered for the
//!    lifetime of the returned handle.
//! 2. When BlueZ needs a user response during pairing it invokes one of
//!    the methods on our exported object (`RequestPasskey`,
//!    `RequestConfirmation`, …). The implementation forwards a
//!    [`PairingPrompt`] on the prompt channel and parks the DBus task on
//!    a [`tokio::sync::oneshot`] until the GUI replies.
//! 3. On drop we call `UnregisterAgent` so BlueZ restores the previous
//!    default agent (gnome-bluetooth, bluetoothctl, …). This is
//!    best-effort: if our process panics or is SIGKILL'd, BlueZ detects
//!    the bus owner disappearing and reverts on its own.
//!
//! ### Threading
//!
//! Agent methods are async and run on the caller's tokio runtime via
//! zbus's `tokio` feature. Forwarding to the GUI is via
//! [`mpsc::Sender`]; the GUI replies via [`oneshot::Sender`]. If no GUI
//! is subscribed (e.g. headless CLI), the agent returns
//! `org.bluez.Error.Rejected` so BlueZ falls back to whatever default
//! agent was previously installed.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, info, warn};
use zbus::{Connection, fdo, interface, names::OwnedBusName, zvariant::OwnedObjectPath};

use crate::error::{Error, Result};

/// DBus object path our agent is exported at.
const AGENT_PATH: &str = "/com/voicetastic/agent";
/// BlueZ DBus service name.
const BLUEZ_SERVICE: &str = "org.bluez";
/// How long to wait for a GUI reply before timing out and rejecting the
/// pairing attempt. Long enough that the user has time to read the
/// passkey on the radio's OLED, short enough that an abandoned prompt
/// doesn't leave BlueZ wedged.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

/// One pairing question forwarded from BlueZ to the GUI.
#[derive(Debug)]
pub struct PairingPrompt {
    /// DBus object path of the `Device1` BlueZ is trying to pair. Useful
    /// for reverse-lookup, but for display we usually prefer the
    /// human-readable `address` extracted from it.
    pub device_path: String,
    /// BD_ADDR of the device, e.g. `"AA:BB:CC:DD:EE:FF"`. Derived from
    /// `device_path` (the last path component, with underscores
    /// translated back to colons).
    pub address: String,
    pub kind: PairingPromptKind,
    /// One-shot reply channel. The GUI sends the user's answer here.
    /// Dropping the receiver without sending is treated as a rejection.
    pub reply: oneshot::Sender<PairingResponse>,
}

#[derive(Debug, Clone)]
pub enum PairingPromptKind {
    /// Legacy PIN (typically a 4–16 digit string).
    PinCode,
    /// 6-digit numeric passkey — what Meshtastic uses.
    Passkey,
    /// Device proposes a passkey; user just confirms yes/no.
    Confirmation(u32),
    /// Service-level authorisation request.
    Authorization { uuid: String },
}

#[derive(Debug, Clone)]
pub enum PairingResponse {
    Pin(String),
    Passkey(u32),
    Confirm(bool),
    /// User pressed cancel or the dialog timed out. Returned to BlueZ as
    /// `org.bluez.Error.Rejected`, which aborts the pairing attempt
    /// cleanly without leaving a half-bonded device behind.
    Cancel,
}

/// Handle to a registered pairing agent. Dropping it unregisters the
/// agent from BlueZ.
pub struct PairingAgent {
    conn: Connection,
    /// Kept alive so its [`Drop`] runs `UnregisterAgent`.
    _guard: AgentGuard,
}

struct AgentGuard {
    conn: Connection,
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        // Fire-and-forget unregister on a detached task. BlueZ will also
        // GC on bus-owner disappearance, so this is a politeness, not a
        // correctness requirement.
        let conn = self.conn.clone();
        tokio::spawn(async move {
            if let Ok(proxy) = AgentManager1Proxy::new(&conn).await {
                let path = OwnedObjectPath::try_from(AGENT_PATH).unwrap();
                let _ = proxy.unregister_agent(&path).await;
            }
        });
    }
}

impl PairingAgent {
    /// Connect to the system bus, export `org.bluez.Agent1` at
    /// [`AGENT_PATH`], and become BlueZ's default agent. Returns the
    /// receiver side of the prompt channel — feed it to the GUI.
    pub async fn install() -> Result<(Self, mpsc::Receiver<PairingPrompt>)> {
        let (tx, rx) = mpsc::channel(4);
        let conn = Connection::system()
            .await
            .map_err(|e| Error::Other(format!("DBus system bus: {e}")))?;

        let agent = Agent1 {
            prompts: Arc::new(Mutex::new(tx)),
        };
        conn.object_server()
            .at(AGENT_PATH, agent)
            .await
            .map_err(|e| Error::Other(format!("export Agent1: {e}")))?;

        let mgr = AgentManager1Proxy::new(&conn)
            .await
            .map_err(|e| Error::Other(format!("AgentManager1 proxy: {e}")))?;
        let path = OwnedObjectPath::try_from(AGENT_PATH)
            .map_err(|e| Error::Other(format!("bad agent path: {e}")))?;
        mgr.register_agent(&path, "KeyboardDisplay")
            .await
            .map_err(|e| Error::Other(format!("RegisterAgent failed: {e}")))?;
        // Best-effort: if another default agent is already pinned by
        // policy, RequestDefaultAgent may fail; pairing still works as
        // long as we are *an* agent.
        if let Err(e) = mgr.request_default_agent(&path).await {
            warn!(error = %e, "RequestDefaultAgent failed (continuing as non-default)");
        }
        info!("BlueZ pairing agent registered at {AGENT_PATH}");

        let guard = AgentGuard { conn: conn.clone() };
        Ok((
            Self {
                conn,
                _guard: guard,
            },
            rx,
        ))
    }

    /// Borrow the underlying DBus connection for direct BlueZ calls
    /// (`Device1.Pair`, etc).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

/// The exported agent object.
struct Agent1 {
    /// Sender end of the prompt channel. Wrapped in a `Mutex` only to
    /// satisfy zbus's `Sync` requirement on the interface state; the
    /// channel itself is internally synchronised.
    prompts: Arc<Mutex<mpsc::Sender<PairingPrompt>>>,
}

impl Agent1 {
    fn address_from_path(path: &str) -> String {
        // BlueZ encodes BD_ADDRs in object paths as
        // `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF`.
        path.rsplit('/')
            .next()
            .and_then(|s| s.strip_prefix("dev_"))
            .map(|s| s.replace('_', ":"))
            .unwrap_or_default()
    }

    async fn ask(
        &self,
        device: &OwnedObjectPath,
        kind: PairingPromptKind,
    ) -> Option<PairingResponse> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let prompt = PairingPrompt {
            device_path: device.as_str().to_string(),
            address: Self::address_from_path(device.as_str()),
            kind,
            reply: reply_tx,
        };
        // Snapshot the sender so we don't hold the mutex across the await.
        let sender = self.prompts.lock().await.clone();
        if sender.send(prompt).await.is_err() {
            warn!("no pairing-prompt subscriber; rejecting");
            return None;
        }
        match tokio::time::timeout(PROMPT_TIMEOUT, reply_rx).await {
            Ok(Ok(resp)) => Some(resp),
            Ok(Err(_)) => {
                warn!("pairing-prompt reply channel dropped");
                None
            }
            Err(_) => {
                warn!("pairing-prompt timed out after {PROMPT_TIMEOUT:?}");
                None
            }
        }
    }
}

#[interface(name = "org.bluez.Agent1")]
impl Agent1 {
    async fn release(&self) {
        debug!("Agent1.Release");
    }

    async fn request_pin_code(&self, device: OwnedObjectPath) -> fdo::Result<String> {
        match self.ask(&device, PairingPromptKind::PinCode).await {
            Some(PairingResponse::Pin(s)) => Ok(s),
            _ => Err(fdo::Error::Failed("rejected".into())),
        }
    }

    async fn display_pin_code(
        &self,
        _device: OwnedObjectPath,
        _pincode: String,
    ) -> fdo::Result<()> {
        // We are KeyboardDisplay but Meshtastic never asks us to display
        // a PIN; accept silently for protocol compliance.
        Ok(())
    }

    async fn request_passkey(&self, device: OwnedObjectPath) -> fdo::Result<u32> {
        match self.ask(&device, PairingPromptKind::Passkey).await {
            Some(PairingResponse::Passkey(p)) => Ok(p),
            _ => Err(fdo::Error::Failed("rejected".into())),
        }
    }

    async fn display_passkey(
        &self,
        _device: OwnedObjectPath,
        _passkey: u32,
        _entered: u16,
    ) -> fdo::Result<()> {
        Ok(())
    }

    async fn request_confirmation(&self, device: OwnedObjectPath, passkey: u32) -> fdo::Result<()> {
        match self
            .ask(&device, PairingPromptKind::Confirmation(passkey))
            .await
        {
            Some(PairingResponse::Confirm(true)) => Ok(()),
            _ => Err(fdo::Error::Failed("rejected".into())),
        }
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> fdo::Result<()> {
        match self
            .ask(
                &device,
                PairingPromptKind::Authorization {
                    uuid: String::new(),
                },
            )
            .await
        {
            Some(PairingResponse::Confirm(true)) => Ok(()),
            _ => Err(fdo::Error::Failed("rejected".into())),
        }
    }

    async fn authorize_service(&self, device: OwnedObjectPath, uuid: String) -> fdo::Result<()> {
        match self
            .ask(&device, PairingPromptKind::Authorization { uuid })
            .await
        {
            Some(PairingResponse::Confirm(true)) => Ok(()),
            _ => Err(fdo::Error::Failed("rejected".into())),
        }
    }

    async fn cancel(&self) {
        debug!("Agent1.Cancel");
    }
}

#[zbus::proxy(
    interface = "org.bluez.AgentManager1",
    default_service = "org.bluez",
    default_path = "/org/bluez"
)]
trait AgentManager1 {
    fn register_agent(&self, agent: &OwnedObjectPath, capability: &str) -> zbus::Result<()>;
    fn unregister_agent(&self, agent: &OwnedObjectPath) -> zbus::Result<()>;
    fn request_default_agent(&self, agent: &OwnedObjectPath) -> zbus::Result<()>;
}

#[zbus::proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
pub trait Device1 {
    fn pair(&self) -> zbus::Result<()>;
    fn cancel_pairing(&self) -> zbus::Result<()>;
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn paired(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn trusted(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_trusted(&self, value: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
}

#[zbus::proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
pub trait Adapter1 {
    fn remove_device(&self, device: &OwnedObjectPath) -> zbus::Result<()>;
}

/// Drive a full `pair → trust → connect` sequence over DBus for the
/// given BD_ADDR (uppercase, colon-separated). The system-bus
/// connection is supplied by the caller so we reuse the same one the
/// pairing agent is registered on (otherwise our agent wouldn't be the
/// one BlueZ calls back into).
///
/// Idempotent: returns `Ok(())` if the device is already paired and
/// connected. On `AuthenticationFailed` (stale bond) we remove the
/// device and retry once.
pub async fn pair_and_connect(conn: &Connection, address: &str) -> Result<()> {
    let upper = address.to_ascii_uppercase();
    let (adapter_path, device_path) = find_device(conn, &upper).await?;
    info!(address = %upper, path = %device_path.as_str(), "pairing via DBus");

    let dev = Device1Proxy::builder(conn)
        .path(&device_path)
        .map_err(|e| Error::Other(format!("device path: {e}")))?
        .build()
        .await
        .map_err(|e| Error::Other(format!("Device1 proxy: {e}")))?;

    let try_pair = || async {
        match dev.pair().await {
            Ok(()) => Ok(()),
            Err(zbus::Error::MethodError(name, msg, _))
                if name.as_str() == "org.bluez.Error.AlreadyExists" =>
            {
                debug!("device already paired ({msg:?})");
                Ok(())
            }
            Err(e) => Err(e),
        }
    };

    if let Err(e) = try_pair().await {
        warn!(error = %e, "initial pair failed; removing stale bond and retrying");
        if let Ok(builder) = Adapter1Proxy::builder(conn).path(&adapter_path)
            && let Ok(adapter) = builder.build().await
        {
            let _ = adapter.remove_device(&device_path).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Re-resolve the device path because RemoveDevice tears the
        // object down; BlueZ recreates it on the next discovery.
        let (_, device_path2) = find_device(conn, &upper).await?;
        let dev2 = Device1Proxy::builder(conn)
            .path(&device_path2)
            .map_err(|e| Error::Other(format!("device path: {e}")))?
            .build()
            .await
            .map_err(|e| Error::Other(format!("Device1 proxy: {e}")))?;
        dev2.pair()
            .await
            .map_err(|e| Error::Other(format!("pair retry failed: {e}")))?;
    }

    // Best-effort trust.
    let _ = dev.set_trusted(true).await;

    match dev.connect().await {
        Ok(()) => {}
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.bluez.Error.AlreadyConnected" => {}
        Err(e) => return Err(Error::Other(format!("Device1.Connect failed: {e}"))),
    }

    info!(address = %upper, "BLE link prepared via DBus");
    Ok(())
}

/// Walk the `org.freedesktop.DBus.ObjectManager` tree under `org.bluez`
/// to find the (adapter, device) object paths matching `address`.
async fn find_device(
    conn: &Connection,
    address: &str,
) -> Result<(OwnedObjectPath, OwnedObjectPath)> {
    let bus = OwnedBusName::try_from(BLUEZ_SERVICE)
        .map_err(|e| Error::Other(format!("bad bus name: {e}")))?;
    let om = zbus::fdo::ObjectManagerProxy::builder(conn)
        .destination(bus)
        .map_err(|e| Error::Other(format!("ObjectManager dest: {e}")))?
        .path("/")
        .map_err(|e| Error::Other(format!("ObjectManager path: {e}")))?
        .build()
        .await
        .map_err(|e| Error::Other(format!("ObjectManager proxy: {e}")))?;
    let objs = om
        .get_managed_objects()
        .await
        .map_err(|e| Error::Other(format!("GetManagedObjects: {e}")))?;
    let upper = address.to_ascii_uppercase();
    let needle_suffix = format!("dev_{}", upper.replace(':', "_"));
    for (path, ifaces) in &objs {
        if ifaces.contains_key("org.bluez.Device1") && path.as_str().ends_with(&needle_suffix) {
            // Adapter path is the parent of the device path.
            let s = path.as_str();
            let parent = s.rsplit_once('/').map(|(p, _)| p).unwrap_or(s);
            let adapter = OwnedObjectPath::try_from(parent)
                .map_err(|e| Error::Other(format!("parent path: {e}")))?;
            return Ok((adapter, path.clone()));
        }
    }
    // Fall back to default adapter if the device hasn't been discovered
    // yet — caller should have run a scan first.
    Err(Error::Other(format!(
        "no BlueZ Device1 object for {upper}; scan first"
    )))
}

#[cfg(test)]
mod tests {
    use super::Agent1;

    #[test]
    fn parses_bd_addr_from_bluez_path() {
        let p = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
        assert_eq!(Agent1::address_from_path(p), "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn empty_for_bad_path() {
        assert_eq!(Agent1::address_from_path("/foo/bar"), "");
    }
}
