//! The sans-IO Meshtastic protocol core — the shared driver contract.
//!
//! This module holds every piece of Meshtastic protocol handling that does not
//! touch I/O or a runtime, so there is exactly one implementation across all
//! clients. A *driver* wires it to a platform: [`super::MeshtasticService`] is
//! the native (tokio) driver; a browser client drives the same surface with
//! Web Serial + `spawn_local`. The contract a driver implements against:
//!
//! - [`decode_inbound`] — one `FromRadio` frame → [`InboundEvent`]s (pure).
//! - [`ProtocolState`] — the canonical config/identity snapshot, updated by
//!   [`ProtocolState::apply`]; the driver decides how to surface it.
//! - [`want_config`] / [`text_packet`] / [`data_packet`] / [`admin_packet`] —
//!   build a `to_radio::PayloadVariant` for the driver to encode + write.
//! - [`crate::voice::tx_policy`] — voice-burst pacing / backpressure decisions.
//!
//! The driver supplies the runtime-owned bits (transport read/write, timers,
//! the packet-id counter, how events reach the UI) and nothing else.
//!
//! Tracing calls stay here (tracing is runtime-agnostic — no tokio), so
//! decode-time observability is unchanged. Effects that depend on *driver*
//! state (e.g. the config-burst-completeness warning, which reads the native
//! driver's config channels) live in the driver instead.

use std::collections::HashMap;

use prost::Message as _;
use tracing::{debug, warn};

use crate::error::{Error, Result};
use crate::ids::node_num_to_id;
use crate::meshtastic::ack::AckResult;
use crate::meshtastic::pkc;
use crate::node::NodeId;
use crate::ports::{ADMIN_APP, BROADCAST_ADDR, MAX_TEXT_BYTES, PRIVATE_APP, TEXT_MESSAGE_APP};
use crate::proto::{
    AdminMessage, Channel, Data, DeviceMetadata, FromRadio, MeshPacket, MyNodeInfo, NodeInfo,
    PortNum, Routing, User, admin_message, config,
    config::{
        BluetoothConfig, DeviceConfig, DisplayConfig, LoRaConfig, NetworkConfig, PositionConfig,
        PowerConfig,
    },
    from_radio, mesh_packet, module_config,
    module_config::MqttConfig,
    routing, to_radio,
};
use crate::voice::types::VoiceData;
use crate::voice::{PROTOCOL_VERSION as VOICE_PROTOCOL_VERSION, VoiceDestination, detect_version};

use super::types::{IncomingData, IncomingText, QueueStatusEvent};

/// Latitude bounds in fixed-point 1e-7 degrees: [-90°, +90°].
const LAT_I_MIN: i32 = -900_000_000;
const LAT_I_MAX: i32 = 900_000_000;
/// Longitude bounds in fixed-point 1e-7 degrees: [-180°, +180°].
const LON_I_MIN: i32 = -1_800_000_000;
const LON_I_MAX: i32 = 1_800_000_000;

fn lat_i_in_range(v: i32) -> bool {
    (LAT_I_MIN..=LAT_I_MAX).contains(&v)
}
fn lon_i_in_range(v: i32) -> bool {
    (LON_I_MIN..=LON_I_MAX).contains(&v)
}

/// Read-only state the decoder needs. Snapshotted by the driver before each
/// call (there is no intra-message state dependency: one `FromRadio` carries
/// exactly one variant).
pub struct InboundCtx<'a> {
    /// Our own node number, if a prior `MyNodeInfo` established it. Used to
    /// recognise our own `NodeInfo` and surface its `User` as the owner.
    pub my_node_num: Option<u32>,
    /// Our node's X25519 private key, captured from `Config::Security`.
    /// Required to decrypt `MeshPacket::Encrypted` PKC DMs whose firmware-
    /// side decrypt failed (typically because the sender's public key was
    /// missing from the radio's nodeDB at decrypt time). `None` until the
    /// config burst has delivered `Security`, or when PKC is disabled on
    /// the device.
    pub our_private_key: Option<&'a [u8; 32]>,
    /// The driver's current node roster — used to look up the sender's
    /// public key when attempting PKC decrypt.
    pub nodes: &'a HashMap<u32, NodeInfo>,
    /// Ring buffer of recently host-decrypted PKC `(from, packet_id)` pairs.
    /// Passed in from the driver so the decoder can check and update it while
    /// `ProtocolState` is already locked (the two mutexes are independent).
    /// `None` in tests and in the wasm build that has no PKC.
    pub pkc_seen: Option<&'a parking_lot::Mutex<std::collections::VecDeque<(u32, u32)>>>,
}

/// A decoded inbound effect for the driver to apply to its channels.
///
/// Mirrors exactly what the old `handle_from_radio` published; the driver's
/// `apply_inbound` is the inverse mapping back onto the watch/broadcast
/// senders.
pub enum InboundEvent {
    MyInfo(MyNodeInfo),
    NodeInfo(NodeInfo),
    /// Owner `User` (our own node's `NodeInfo`, or an admin `GetOwnerResponse`).
    Owner(User),
    /// One of the seven tracked config sections.
    Config(config::PayloadVariant),
    /// One tracked module-config section. Currently only the MQTT
    /// variant is surfaced; other module-config variants are silently
    /// dropped by `decode_inbound`. Add them here + `apply_module_config`
    /// when their UI lands.
    ModuleConfig(module_config::PayloadVariant),
    Channel(Channel),
    Metadata(DeviceMetadata),
    ConfigComplete(u32),
    IncomingText(IncomingText),
    IncomingData(IncomingData),
    Voice(VoiceData),
    QueueStatus(QueueStatusEvent),
    /// Firmware delivery report for an outbound packet we sent with
    /// `want_ack`. `request_id` is the original packet's id;
    /// `result` carries success or a typed failure.
    AckOrNak {
        request_id: u32,
        result: AckResult,
    },
}

impl InboundEvent {
    /// True if this event updates the canonical config/identity snapshot
    /// ([`ProtocolState`]); false for transient messages (text/data/voice)
    /// and queue status, which the driver routes straight to its broadcast /
    /// notify channels.
    pub fn is_snapshot(&self) -> bool {
        matches!(
            self,
            Self::MyInfo(_)
                | Self::NodeInfo(_)
                | Self::Owner(_)
                | Self::Config(_)
                | Self::ModuleConfig(_)
                | Self::Channel(_)
                | Self::Metadata(_)
        )
    }
}

/// Decode one `FromRadio` frame into driver effects. Pure: no I/O, no awaits.
pub fn decode_inbound(bytes: &[u8], ctx: &InboundCtx) -> Result<Vec<InboundEvent>> {
    let msg = FromRadio::decode(bytes)?;
    let mut out = Vec::new();
    let Some(variant) = msg.payload_variant else {
        return Ok(out);
    };
    match variant {
        from_radio::PayloadVariant::MyInfo(info) => {
            debug!(my_node_num = info.my_node_num, "MyNodeInfo");
            out.push(InboundEvent::MyInfo(info));
        }
        from_radio::PayloadVariant::NodeInfo(ni) => {
            let mut ni = ni;
            // Sanitise position fields against absurd values from a
            // misbehaving radio so downstream UI never sees garbage.
            if let Some(pos) = ni.position.as_mut() {
                if let Some(lat) = pos.latitude_i
                    && !lat_i_in_range(lat)
                {
                    warn!(node = ni.num, lat, "dropping out-of-range latitude_i");
                    pos.latitude_i = None;
                }
                if let Some(lon) = pos.longitude_i
                    && !lon_i_in_range(lon)
                {
                    warn!(node = ni.num, lon, "dropping out-of-range longitude_i");
                    pos.longitude_i = None;
                }
            }
            // If this is our own node, surface the User as the "owner".
            if Some(ni.num) == ctx.my_node_num
                && let Some(user) = ni.user.as_ref()
            {
                out.push(InboundEvent::Owner(user.clone()));
            }
            out.push(InboundEvent::NodeInfo(ni));
        }
        from_radio::PayloadVariant::Config(cfg) => {
            if let Some(v) = cfg.payload_variant {
                // Only the seven sections the service tracks produce events.
                match v {
                    config::PayloadVariant::Lora(_)
                    | config::PayloadVariant::Device(_)
                    | config::PayloadVariant::Position(_)
                    | config::PayloadVariant::Power(_)
                    | config::PayloadVariant::Network(_)
                    | config::PayloadVariant::Display(_)
                    | config::PayloadVariant::Bluetooth(_) => out.push(InboundEvent::Config(v)),
                    _ => {}
                }
            }
        }
        from_radio::PayloadVariant::ModuleConfig(mc) => {
            if let Some(v) = mc.payload_variant
                && matches!(v, module_config::PayloadVariant::Mqtt(_))
            {
                out.push(InboundEvent::ModuleConfig(v));
            }
        }
        from_radio::PayloadVariant::Channel(ch) => out.push(InboundEvent::Channel(ch)),
        from_radio::PayloadVariant::Metadata(meta) => out.push(InboundEvent::Metadata(meta)),
        from_radio::PayloadVariant::ConfigCompleteId(nonce) => {
            out.push(InboundEvent::ConfigComplete(nonce));
        }
        from_radio::PayloadVariant::Packet(pkt) => decode_packet(pkt, ctx, &mut out),
        from_radio::PayloadVariant::QueueStatus(qs) => {
            debug!(
                free = qs.free,
                maxlen = qs.maxlen,
                res = qs.res,
                pkt = qs.mesh_packet_id,
                "queue_status"
            );
            out.push(InboundEvent::QueueStatus(QueueStatusEvent {
                res: qs.res,
                free: qs.free,
                maxlen: qs.maxlen,
                mesh_packet_id: qs.mesh_packet_id,
            }));
        }
        _ => {}
    }
    Ok(out)
}

fn decode_packet(pkt: MeshPacket, ctx: &InboundCtx, out: &mut Vec<InboundEvent>) {
    // Snapshot the header fields up front: the `match` on `payload_variant`
    // moves it out of `pkt`, which would prevent further `pkt.field` access
    // by-reference on the encrypted-arm rescue path.
    let from = pkt.from;
    let to = pkt.to;
    let id = pkt.id;
    let channel = pkt.channel;

    // Destructure `payload_variant` by value so the payload can be moved
    // through this function instead of cloned at entry.
    let data = match pkt.payload_variant {
        Some(mesh_packet::PayloadVariant::Decoded(d)) => d,
        Some(mesh_packet::PayloadVariant::Encrypted(bytes)) => {
            // Channel-encrypted packets always arrive `Decoded` because the
            // firmware unwraps them locally with the loaded PSK. So an
            // `Encrypted` arm at this point is the firmware telling us "I
            // couldn't decrypt this either" — either an overheard PKC DM
            // between other nodes (we have no way in) or a PKC DM to us
            // whose sender's public key wasn't in the radio's nodeDB at
            // decrypt time. The latter we can rescue from the host if the
            // config burst has delivered our private key and the sender's
            // public key is in our local nodes table.
            match try_pkc_decrypt(from, to, id, channel, &bytes, ctx) {
                Some(d) => d,
                None => return,
            }
        }
        None => return,
    };
    let portnum = data.portnum;
    let mut payload = data.payload;
    // Admin responses (e.g. get_owner_response) come back as a packet on
    // ADMIN_APP. Decode them so the settings UI sees the latest values.
    if portnum == PortNum::AdminApp as i32
        && let Ok(admin) = AdminMessage::decode(payload.as_slice())
        && let Some(v) = admin.payload_variant
    {
        match v {
            admin_message::PayloadVariant::GetOwnerResponse(user) => {
                out.push(InboundEvent::Owner(user));
            }
            admin_message::PayloadVariant::GetChannelResponse(ch) => {
                out.push(InboundEvent::Channel(ch));
            }
            admin_message::PayloadVariant::GetDeviceMetadataResponse(meta) => {
                out.push(InboundEvent::Metadata(meta));
            }
            _ => {}
        }
        return;
    }
    // Routing-app responses are the firmware's delivery report for an
    // outbound packet we sent with `want_ack`. `data.request_id` matches
    // the original outgoing packet id; the inner `Routing.variant`
    // carries success or a typed failure. We only emit the AckOrNak
    // event — these don't surface as IncomingText/IncomingData since
    // they're protocol control, not application payload.
    if portnum == PortNum::RoutingApp as i32 {
        if data.request_id == 0 {
            // Not an ack/nak — could be a route_request / route_reply.
            return;
        }
        let result = match Routing::decode(payload.as_slice()) {
            Ok(r) => match r.variant {
                Some(routing::Variant::ErrorReason(e)) => match routing::Error::try_from(e) {
                    Ok(routing::Error::None) => AckResult::Delivered,
                    Ok(err) => AckResult::Failed(err),
                    // Unknown enum value from a newer firmware: treat as
                    // failed but with a generic NoRoute so callers see a
                    // non-Delivered result. Better than dropping silently.
                    Err(_) => AckResult::Failed(routing::Error::NoRoute),
                },
                // RouteDiscovery responses, not delivery acks. Ignore.
                _ => return,
            },
            Err(e) => {
                warn!(
                    request_id = data.request_id,
                    error = %e,
                    "malformed Routing payload on ROUTING_APP; dropping",
                );
                return;
            }
        };
        out.push(InboundEvent::AckOrNak {
            request_id: data.request_id,
            result,
        });
        return;
    }
    if portnum == PortNum::TextMessageApp as i32 {
        if payload.len() > MAX_TEXT_BYTES {
            warn!(
                from = pkt.from,
                len = payload.len(),
                "dropping oversized text payload"
            );
            return;
        }
        match String::from_utf8(payload) {
            Ok(text) => {
                let from_id = node_num_to_id(pkt.from);
                out.push(InboundEvent::IncomingText(IncomingText {
                    from: pkt.from,
                    from_id,
                    to: pkt.to,
                    channel: pkt.channel,
                    text,
                    rx_time: pkt.rx_time,
                    rx_snr: pkt.rx_snr,
                    rx_rssi: pkt.rx_rssi,
                }));
                return;
            }
            Err(e) => {
                warn!(
                    from = pkt.from,
                    len = e.as_bytes().len(),
                    "malformed UTF-8 on TextMessageApp; falling through to data fan-out"
                );
                payload = e.into_bytes();
            }
        }
    }
    // Tap PRIVATE_APP voice frames matching our wire version onto the
    // protocol-agnostic voice channel. Legacy consumers still receive the
    // same bytes via the IncomingData event below.
    //
    // (The old code skipped these emits when nobody was subscribed and used a
    // take-vs-clone trick to avoid one copy; that's a driver-side optimisation
    // — the driver still gates the broadcast on `receiver_count`, it just no
    // longer saves the single clone. Behaviour to subscribers is identical.)
    let is_voice =
        portnum == PRIVATE_APP as i32 && detect_version(&payload) == Some(VOICE_PROTOCOL_VERSION);
    if is_voice {
        let dest = if pkt.to == BROADCAST_ADDR {
            VoiceDestination::Broadcast
        } else {
            VoiceDestination::Node(NodeId::from_u32(pkt.to))
        };
        out.push(InboundEvent::Voice(VoiceData {
            from: NodeId::from_u32(pkt.from),
            to: dest,
            channel: pkt.channel,
            payload: payload.clone(),
        }));
    }
    out.push(InboundEvent::IncomingData(IncomingData {
        from: pkt.from,
        to: pkt.to,
        channel: pkt.channel,
        portnum,
        payload,
        rx_time: pkt.rx_time,
    }));
}

/// Attempt to PKC-decrypt an `Encrypted` `MeshPacket` whose `to` is us.
/// Returns the recovered `meshtastic.Data` on success, or `None` if the
/// packet isn't actually a PKC DM addressed to us, we lack key material,
/// or the decrypt / inner-protobuf-decode fails. Never panics; never logs
/// the keys themselves.
fn try_pkc_decrypt(
    from: u32,
    to: u32,
    id: u32,
    channel: u32,
    ciphertext: &[u8],
    ctx: &InboundCtx,
) -> Option<Data> {
    let my = ctx.my_node_num?;
    if to != my {
        // Overheard PKC DM between other nodes — we don't have the
        // recipient's private key and couldn't decrypt even if we tried.
        // Demoted to trace (was a noisy debug previously).
        tracing::trace!(
            from,
            to,
            channel,
            len = ciphertext.len(),
            "drop encrypted MeshPacket: not addressed to us",
        );
        return None;
    }
    let our_private = ctx.our_private_key?;
    let peer_user = ctx.nodes.get(&from).and_then(|n| n.user.as_ref())?;
    if peer_user.public_key.len() != 32 {
        // Either PKC disabled on the sender's side, or we haven't seen
        // their `NodeInfo` yet. Either way nothing the host can do.
        tracing::debug!(
            from,
            "drop PKC DM to us: sender public_key unknown (size={})",
            peer_user.public_key.len(),
        );
        return None;
    }
    let mut peer_public = [0u8; 32];
    peer_public.copy_from_slice(&peer_user.public_key);

    // Dedup: firmware flood-retransmits may re-deliver the same ciphertext
    // multiple times. Check before the (cheap but non-free) decrypt so
    // replays short-circuit immediately.
    let mut seen_guard = ctx.pkc_seen.map(|s| s.lock());
    if seen_guard.as_ref().is_some_and(|g| g.contains(&(from, id))) {
        tracing::debug!(from, id, "drop PKC DM: replay dedup");
        return None;
    }

    let plaintext = pkc::decrypt(our_private, &peer_public, from, id, ciphertext)?;
    match Data::decode(plaintext.as_slice()) {
        Ok(d) => {
            tracing::debug!(from, id, portnum = d.portnum, "host-decrypted PKC DM",);
            if let Some(ref mut g) = seen_guard {
                g.push_front((from, id));
                if g.len() > 64 {
                    g.pop_back();
                }
            }
            Some(d)
        }
        Err(e) => {
            tracing::debug!(
                from,
                id,
                error = %e,
                "PKC decrypt succeeded but inner Data decode failed",
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Outbound construction (pure).
//
// Each builder produces a `to_radio::PayloadVariant` ready to encode + write.
// The driver supplies the runtime-owned bits — the packet `id` (from the
// service's atomic counter) and, for admin writes, the destination node — and
// performs the actual transport write. No I/O here.
// ---------------------------------------------------------------------------

/// `WantConfigId` handshake payload.
pub fn want_config(nonce: u32) -> to_radio::PayloadVariant {
    to_radio::PayloadVariant::WantConfigId(nonce)
}

/// A `TextMessageApp` packet. `to` defaults to broadcast; `want_ack` is set
/// only for direct messages. Fails if the text exceeds the firmware limit.
pub fn text_packet(
    id: u32,
    text: &str,
    channel: u32,
    to: Option<u32>,
) -> Result<to_radio::PayloadVariant> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(Error::Other(format!(
            "text payload too large: {} > {MAX_TEXT_BYTES} bytes",
            text.len()
        )));
    }
    let pkt = MeshPacket {
        from: 0,
        to: to.unwrap_or(BROADCAST_ADDR),
        channel,
        id,
        want_ack: to.is_some(),
        hop_limit: 3,
        priority: mesh_packet::Priority::Default as i32,
        payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
            portnum: TEXT_MESSAGE_APP as i32,
            payload: text.as_bytes().to_vec(),
            ..Default::default()
        })),
        ..Default::default()
    };
    Ok(to_radio::PayloadVariant::Packet(pkt))
}

/// A raw application-data packet (e.g. voice chunks on `PRIVATE_APP`).
pub fn data_packet(
    id: u32,
    portnum: i32,
    payload: Vec<u8>,
    channel: u32,
    to: Option<u32>,
    want_ack: bool,
    want_response: bool,
) -> to_radio::PayloadVariant {
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
            want_response,
            ..Default::default()
        })),
        ..Default::default()
    };
    to_radio::PayloadVariant::Packet(pkt)
}

/// An `AdminMessage` packet addressed to `to_node` (our own node, for config
/// writes). Reliable priority, `want_ack`, no app-level response requested.
pub fn admin_packet(
    id: u32,
    to_node: u32,
    payload: admin_message::PayloadVariant,
) -> Result<to_radio::PayloadVariant> {
    let admin = AdminMessage {
        payload_variant: Some(payload),
        ..Default::default()
    };
    let mut bytes = Vec::with_capacity(admin.encoded_len());
    admin.encode(&mut bytes)?;
    let pkt = MeshPacket {
        from: 0,
        to: to_node,
        channel: 0,
        id,
        want_ack: true,
        hop_limit: 0,
        priority: mesh_packet::Priority::Reliable as i32,
        payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
            portnum: ADMIN_APP as i32,
            payload: bytes,
            want_response: false,
            ..Default::default()
        })),
        ..Default::default()
    };
    Ok(to_radio::PayloadVariant::Packet(pkt))
}

/// Active node-discovery ping: broadcast our `User` on `NODEINFO_APP` with
/// `want_response = true` so peers reply with their own `User` immediately
/// instead of waiting for the next periodic NodeInfo broadcast. Their replies
/// hit the firmware's NodeDB and arrive back to us as `FromRadio::NodeInfo`
/// events, just like passive discovery — this only accelerates it.
pub fn nodeinfo_request_packet(
    id: u32,
    owner: &User,
    channel: u32,
) -> Result<to_radio::PayloadVariant> {
    let mut payload = Vec::with_capacity(owner.encoded_len());
    owner.encode(&mut payload)?;
    let pkt = MeshPacket {
        from: 0,
        to: BROADCAST_ADDR,
        channel,
        id,
        want_ack: false,
        hop_limit: 3,
        priority: mesh_packet::Priority::Default as i32,
        payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
            portnum: PortNum::NodeinfoApp as i32,
            payload,
            want_response: true,
            ..Default::default()
        })),
        ..Default::default()
    };
    Ok(to_radio::PayloadVariant::Packet(pkt))
}

// ---------------------------------------------------------------------------
// Canonical protocol state (sans-IO).
//
// The single source of truth for the device's config/identity snapshot, with
// one implementation of every state transition ([`ProtocolState::apply`]) that
// all drivers share. The native [`super::MeshtasticService`] keeps it in a
// sync `Mutex` and mirrors the touched field into its watch channels (so its
// subscriber API is unchanged); a browser driver can read it directly.
//
// Transient messages, connection state, the outbound packet-id counter, and
// the firmware queue depth are tracked outside this struct (broadcast
// channels / `set_state` / atomics / a dedicated mutex), so they are not
// duplicated here.
// ---------------------------------------------------------------------------

/// The device config + identity snapshot, rebuilt from inbound events.
#[derive(Default)]
pub struct ProtocolState {
    pub my_info: Option<MyNodeInfo>,
    pub nodes: HashMap<u32, NodeInfo>,
    pub owner: Option<User>,
    pub lora: Option<LoRaConfig>,
    pub device: Option<DeviceConfig>,
    pub position: Option<PositionConfig>,
    pub power: Option<PowerConfig>,
    pub network: Option<NetworkConfig>,
    pub display: Option<DisplayConfig>,
    pub bluetooth: Option<BluetoothConfig>,
    /// MQTT module-config snapshot. Tracked because the gateway feature
    /// is one of the few module-configs the UI is expected to edit; the
    /// other ~12 variants are not surfaced yet.
    pub mqtt: Option<MqttConfig>,
    pub channels: Vec<Channel>,
    pub metadata: Option<DeviceMetadata>,
    /// Our X25519 private key, captured from `Config::Security`. Held as
    /// a raw 32-byte array rather than the full `SecurityConfig` proto
    /// because it must not leak into any of the public watch channels.
    /// Inbound-only: the desktop client never sends PKC DMs, so this is
    /// read by [`super::super::pkc::decrypt`] via [`InboundCtx`] and
    /// nowhere else.
    our_private_key: Option<[u8; 32]>,
}

impl ProtocolState {
    /// Apply one snapshot-updating event. Non-snapshot events (see
    /// [`InboundEvent::is_snapshot`]) are ignored. Takes the event by
    /// reference so the driver can still move it into its own channels.
    pub fn apply(&mut self, event: &InboundEvent) {
        match event {
            InboundEvent::MyInfo(info) => self.my_info = Some(info.clone()),
            InboundEvent::NodeInfo(ni) => {
                self.nodes.insert(ni.num, ni.clone());
            }
            InboundEvent::Owner(user) => self.owner = Some(user.clone()),
            InboundEvent::Config(v) => self.apply_config(v.clone()),
            InboundEvent::ModuleConfig(v) => self.apply_module_config(v.clone()),
            InboundEvent::Channel(ch) => self.upsert_channel(ch.clone()),
            InboundEvent::Metadata(meta) => self.metadata = Some(meta.clone()),
            InboundEvent::ConfigComplete(_)
            | InboundEvent::IncomingText(_)
            | InboundEvent::IncomingData(_)
            | InboundEvent::Voice(_)
            | InboundEvent::QueueStatus(_)
            | InboundEvent::AckOrNak { .. } => {}
        }
    }

    /// Returns the local node's full `NodeInfo` entry, or `None` if either
    /// `my_info` or the corresponding `nodes` entry has not arrived yet.
    pub fn self_node(&self) -> Option<&NodeInfo> {
        let num = self.my_info.as_ref()?.my_node_num;
        self.nodes.get(&num)
    }

    fn apply_config(&mut self, v: config::PayloadVariant) {
        match v {
            config::PayloadVariant::Lora(c) => self.lora = Some(c),
            config::PayloadVariant::Device(c) => self.device = Some(c),
            config::PayloadVariant::Position(c) => self.position = Some(c),
            config::PayloadVariant::Power(c) => self.power = Some(c),
            config::PayloadVariant::Network(c) => self.network = Some(c),
            config::PayloadVariant::Display(c) => self.display = Some(c),
            config::PayloadVariant::Bluetooth(c) => self.bluetooth = Some(c),
            config::PayloadVariant::Security(c) => {
                self.our_private_key = match c.private_key.len() {
                    32 => {
                        let mut k = [0u8; 32];
                        k.copy_from_slice(&c.private_key);
                        Some(k)
                    }
                    // PKC disabled on this device, or firmware didn't
                    // populate the field — clear any stale key from a
                    // previous connection. Length-0 is the normal "no
                    // PKC" signal; other non-32 lengths shouldn't occur
                    // on real firmware.
                    _ => None,
                };
            }
            _ => {}
        }
    }

    fn apply_module_config(&mut self, v: module_config::PayloadVariant) {
        // Only the variants the UI tracks are stored on `ProtocolState`;
        // `decode_inbound` already filters everything else out of the
        // inbound event stream so the unknown arm is just defensive.
        if let module_config::PayloadVariant::Mqtt(c) = v {
            self.mqtt = Some(c);
        }
    }

    /// Our X25519 private key snapshot. Available to driver-internal code
    /// for building an [`InboundCtx`]; intentionally not part of the
    /// public watch-channel surface.
    pub(in crate::meshtastic) fn our_private_key(&self) -> Option<&[u8; 32]> {
        self.our_private_key.as_ref()
    }

    /// Insert or replace a channel, keeping the list sorted by index.
    fn upsert_channel(&mut self, ch: Channel) {
        if let Some(slot) = self.channels.iter_mut().find(|c| c.index == ch.index) {
            *slot = ch;
        } else {
            self.channels.push(ch);
            self.channels.sort_by_key(|c| c.index);
        }
    }

    /// Reset the config sections + channel list (but not identity/nodes), as
    /// done when re-requesting the config burst. The PKC private key is
    /// part of the config burst (`Config::Security`) so it goes too — a
    /// fresh burst will reseed it if the device has PKC enabled.
    pub fn clear_config(&mut self) {
        self.lora = None;
        self.device = None;
        self.position = None;
        self.power = None;
        self.network = None;
        self.display = None;
        self.bluetooth = None;
        self.mqtt = None;
        self.channels.clear();
        self.our_private_key = None;
    }

    /// Wipe the learned-peer map (every `(node_num, NodeInfo)` we've
    /// accumulated). Mirrors what the firmware does on
    /// `AdminMessage::NodedbReset` so a client driving the same admin
    /// action can resync its local view in lockstep.
    pub fn clear_nodes(&mut self) {
        self.nodes.clear();
    }
}

#[cfg(test)]
mod tests {
    //! These run with no tokio runtime, no service, no hardware — the point of
    //! pulling the decode logic out into a sans-IO function.
    use super::*;
    use crate::proto::{Position, from_radio};

    /// Build an `InboundCtx` for a test that doesn't exercise the PKC
    /// decrypt path. The caller's `nodes` HashMap must live at least as
    /// long as the returned context.
    fn ctx<'a>(my_node_num: Option<u32>, nodes: &'a HashMap<u32, NodeInfo>) -> InboundCtx<'a> {
        InboundCtx {
            my_node_num,
            our_private_key: None,
            nodes,
            pkc_seen: None,
        }
    }

    fn encode(variant: from_radio::PayloadVariant) -> Vec<u8> {
        let msg = FromRadio {
            id: 0,
            payload_variant: Some(variant),
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf).expect("encode");
        buf
    }

    #[test]
    fn decodes_my_info() {
        let bytes = encode(from_radio::PayloadVariant::MyInfo(MyNodeInfo {
            my_node_num: 0x1234_5678,
            ..Default::default()
        }));
        let nodes = HashMap::new();
        let ev = decode_inbound(&bytes, &ctx(None, &nodes)).unwrap();
        assert!(matches!(
            ev.as_slice(),
            [InboundEvent::MyInfo(i)] if i.my_node_num == 0x1234_5678
        ));
    }

    #[test]
    fn own_nodeinfo_also_yields_owner() {
        let ni = NodeInfo {
            num: 7,
            user: Some(User {
                long_name: "me".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = encode(from_radio::PayloadVariant::NodeInfo(ni));
        let nodes = HashMap::new();
        // Without my_node_num: just the NodeInfo event.
        let ev = decode_inbound(&bytes, &ctx(None, &nodes)).unwrap();
        assert!(matches!(ev.as_slice(), [InboundEvent::NodeInfo(_)]));
        // When it's our own node: Owner is emitted before NodeInfo.
        let ev = decode_inbound(&bytes, &ctx(Some(7), &nodes)).unwrap();
        assert!(matches!(
            ev.as_slice(),
            [InboundEvent::Owner(u), InboundEvent::NodeInfo(_)] if u.long_name == "me"
        ));
    }

    #[test]
    fn text_packet_rejects_oversized() {
        let big = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(text_packet(1, &big, 0, None).is_err());
        assert!(text_packet(1, "hi", 0, None).is_ok());
    }

    #[test]
    fn text_packet_sets_want_ack_only_for_direct() {
        let to_radio::PayloadVariant::Packet(bcast) = text_packet(1, "hi", 0, None).unwrap() else {
            panic!("expected packet");
        };
        assert!(!bcast.want_ack, "broadcast must not request ack");
        let to_radio::PayloadVariant::Packet(dm) = text_packet(1, "hi", 0, Some(42)).unwrap()
        else {
            panic!("expected packet");
        };
        assert!(dm.want_ack, "direct message must request ack");
        assert_eq!(dm.to, 42);
    }

    #[test]
    fn protocol_state_tracks_snapshot() {
        let mut s = ProtocolState::default();
        assert!(s.my_info.is_none());

        // MyInfo + two NodeInfos accumulate.
        s.apply(&InboundEvent::MyInfo(MyNodeInfo {
            my_node_num: 9,
            ..Default::default()
        }));
        s.apply(&InboundEvent::NodeInfo(NodeInfo {
            num: 1,
            ..Default::default()
        }));
        s.apply(&InboundEvent::NodeInfo(NodeInfo {
            num: 2,
            ..Default::default()
        }));
        assert_eq!(s.my_info.as_ref().unwrap().my_node_num, 9);
        assert_eq!(s.nodes.len(), 2);

        // Transient events don't touch the snapshot.
        s.apply(&InboundEvent::ConfigComplete(7));
        assert_eq!(s.nodes.len(), 2);
    }

    #[test]
    fn protocol_state_channels_stay_sorted_and_upsert() {
        let mut s = ProtocolState::default();
        let ch = |index, name: &str| Channel {
            index,
            ..Channel {
                settings: Some(crate::proto::ChannelSettings {
                    name: name.into(),
                    ..Default::default()
                }),
                ..Default::default()
            }
        };
        s.apply(&InboundEvent::Channel(ch(2, "two")));
        s.apply(&InboundEvent::Channel(ch(0, "zero")));
        assert_eq!(
            s.channels.iter().map(|c| c.index).collect::<Vec<_>>(),
            [0, 2]
        );
        // Upsert in place (no duplicate).
        s.apply(&InboundEvent::Channel(ch(0, "zero-v2")));
        assert_eq!(s.channels.len(), 2);
        assert_eq!(s.channels[0].settings.as_ref().unwrap().name, "zero-v2");
    }

    #[test]
    fn clear_config_keeps_identity() {
        let mut s = ProtocolState::default();
        s.apply(&InboundEvent::MyInfo(MyNodeInfo {
            my_node_num: 9,
            ..Default::default()
        }));
        s.apply(&InboundEvent::Config(config::PayloadVariant::Lora(
            Default::default(),
        )));
        assert!(s.lora.is_some());
        s.clear_config();
        assert!(s.lora.is_none(), "config cleared");
        assert!(s.my_info.is_some(), "identity preserved");
    }

    #[test]
    fn out_of_range_latitude_is_dropped() {
        let ni = NodeInfo {
            num: 7,
            position: Some(Position {
                latitude_i: Some(900_000_001), // > 90°
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = encode(from_radio::PayloadVariant::NodeInfo(ni));
        let nodes = HashMap::new();
        let ev = decode_inbound(&bytes, &ctx(None, &nodes)).unwrap();
        let [InboundEvent::NodeInfo(ni)] = ev.as_slice() else {
            panic!("expected NodeInfo");
        };
        assert_eq!(ni.position.as_ref().unwrap().latitude_i, None);
    }

    /// End-to-end PKC integration test: build a `MeshPacket::Encrypted`
    /// using the firmware's documented test vector, hand it through
    /// `decode_inbound`, and confirm the decrypted `Data` reaches the
    /// `IncomingText` event with the right payload. Locks down both the
    /// inner decrypt and the protocol-level plumbing in one go.
    #[test]
    fn host_decrypts_pkc_dm_to_us() {
        let our_node_num = 0xfeed_face;
        let sender = 0x0929;
        let packet_id = 0x13b2_d662_u32;
        // Firmware crypto test vector (test_PKC in
        // firmware/test/test_crypto/test_main.cpp): our private key + the
        // sender's public key + the firmware-encrypted ciphertext.
        let our_private = {
            let mut k = [0u8; 32];
            k.copy_from_slice(
                &hex::decode("a00330633e63522f8a4d81ec6d9d1e6617f6c8ffd3a4c698229537d44e522277")
                    .unwrap(),
            );
            k
        };
        let peer_public =
            hex::decode("db18fc50eea47f00251cb784819a3cf5fc361882597f589f0d7ff820e8064457")
                .unwrap();
        let ciphertext_and_trailer =
            hex::decode("40df24abfcc30a17a3d9046726099e796a1c036a792b").unwrap();
        // The 10-byte plaintext is a `meshtastic.Data` proto: portnum=1
        // (TEXT_MESSAGE_APP), payload="test", want_response=false.
        // (08 01 12 04 "test" 48 00)

        // Stand up a nodes table that says "we have the sender's public key".
        let mut nodes = HashMap::new();
        nodes.insert(
            sender,
            NodeInfo {
                num: sender,
                user: Some(User {
                    public_key: peer_public,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );

        let pkt = MeshPacket {
            from: sender,
            to: our_node_num,
            id: packet_id,
            channel: 0,
            payload_variant: Some(mesh_packet::PayloadVariant::Encrypted(
                ciphertext_and_trailer,
            )),
            ..Default::default()
        };
        let bytes = encode(from_radio::PayloadVariant::Packet(pkt));

        let cx = InboundCtx {
            my_node_num: Some(our_node_num),
            our_private_key: Some(&our_private),
            nodes: &nodes,
            pkc_seen: None,
        };
        let ev = decode_inbound(&bytes, &cx).unwrap();

        // Should produce IncomingText (text channel app) + the generic
        // IncomingData broadcast. We don't assert on order beyond
        // "IncomingText is present somewhere", since `decode_packet`
        // emits both for portnum=TEXT_MESSAGE_APP.
        let text = ev.iter().find_map(|e| match e {
            InboundEvent::IncomingText(t) => Some(t),
            _ => None,
        });
        let text = text.expect("expected IncomingText after host-side PKC decrypt");
        assert_eq!(text.text, "test");
        assert_eq!(text.channel, 0);
    }

    /// A PKC DM whose (from, id) pair is already in the seen-set is dropped
    /// as a replay without re-decrypting. Firmware flood-routing can deliver
    /// the same ciphertext multiple times; only the first should surface.
    #[test]
    fn pkc_replay_is_deduplicated() {
        // Reuse the same packet bytes from `host_decrypts_pkc_dm_to_us`.
        let our_node_num = 0xfeed_face_u32;
        let sender = 0x0929_u32;
        let packet_id = 0x13b2_d662_u32;
        let our_private = {
            let mut k = [0u8; 32];
            k.copy_from_slice(
                &hex::decode("a00330633e63522f8a4d81ec6d9d1e6617f6c8ffd3a4c698229537d44e522277")
                    .unwrap(),
            );
            k
        };
        let peer_public =
            hex::decode("db18fc50eea47f00251cb784819a3cf5fc361882597f589f0d7ff820e8064457")
                .unwrap();
        let ciphertext =
            hex::decode("40df24abfcc30a17a3d9046726099e796a1c036a792b").unwrap();
        let mut nodes = HashMap::new();
        nodes.insert(
            sender,
            NodeInfo {
                num: sender,
                user: Some(User {
                    public_key: peer_public,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let pkt = MeshPacket {
            from: sender,
            to: our_node_num,
            id: packet_id,
            channel: 0,
            payload_variant: Some(mesh_packet::PayloadVariant::Encrypted(ciphertext)),
            ..Default::default()
        };
        let bytes = encode(from_radio::PayloadVariant::Packet(pkt));

        // Pre-populate the seen set with this (from, id) pair.
        let seen: parking_lot::Mutex<std::collections::VecDeque<(u32, u32)>> =
            parking_lot::Mutex::new(std::collections::VecDeque::new());
        seen.lock().push_front((sender, packet_id));

        let cx = InboundCtx {
            my_node_num: Some(our_node_num),
            our_private_key: Some(&our_private),
            nodes: &nodes,
            pkc_seen: Some(&seen),
        };
        let ev = decode_inbound(&bytes, &cx).unwrap();
        assert!(
            ev.is_empty(),
            "replay PKC DM must be dropped silently; got {} events",
            ev.len()
        );
    }

    /// Encrypted packet not addressed to us → dropped without an attempt.
    /// This covers the overheard-PKC-DM case (which dominates real-world
    /// traffic) and ensures we don't even try to decrypt without our
    /// node number matching.
    #[test]
    fn encrypted_not_to_us_is_dropped() {
        let pkt = MeshPacket {
            from: 1,
            to: 2,
            id: 42,
            channel: 0,
            payload_variant: Some(mesh_packet::PayloadVariant::Encrypted(vec![0u8; 30])),
            ..Default::default()
        };
        let bytes = encode(from_radio::PayloadVariant::Packet(pkt));
        let nodes = HashMap::new();
        let ev = decode_inbound(&bytes, &ctx(Some(0xdead), &nodes)).unwrap();
        assert!(
            ev.is_empty(),
            "encrypted packet to others must not emit events"
        );
    }

    /// Inbound `Routing` packet with `error_reason = NONE` and a matching
    /// `request_id` should surface as `InboundEvent::AckOrNak` carrying
    /// `AckResult::Delivered`. Locks down the wire-format contract with
    /// the firmware's `RoutingModule::sendAckNak` path.
    #[test]
    fn routing_app_packet_emits_delivered_ack() {
        use crate::meshtastic::ack::AckResult;
        use crate::ports::ROUTING_APP;

        let request_id = 0xdead_beef_u32;
        // Build the inner `Routing` proto with `ErrorReason(None)`.
        let routing_payload = Routing {
            variant: Some(routing::Variant::ErrorReason(routing::Error::None as i32)),
        };
        let mut buf = Vec::with_capacity(routing_payload.encoded_len());
        routing_payload.encode(&mut buf).unwrap();

        let pkt = MeshPacket {
            from: 7,
            to: 0xfeed_face,
            id: 1,
            channel: 0,
            payload_variant: Some(mesh_packet::PayloadVariant::Decoded(crate::proto::Data {
                portnum: ROUTING_APP as i32,
                payload: buf,
                request_id,
                ..Default::default()
            })),
            ..Default::default()
        };
        let bytes = encode(from_radio::PayloadVariant::Packet(pkt));
        let nodes = HashMap::new();
        let ev = decode_inbound(&bytes, &ctx(Some(0xfeed_face), &nodes)).unwrap();
        let ack = ev
            .iter()
            .find_map(|e| match e {
                InboundEvent::AckOrNak { request_id, result } => Some((*request_id, *result)),
                _ => None,
            })
            .expect("expected AckOrNak event");
        assert_eq!(ack, (request_id, AckResult::Delivered));
    }

    /// Inbound `Routing` packet with a typed error surfaces as
    /// `AckResult::Failed(err)`. Picks `MaxRetransmit` since that's the
    /// common failure on slow LoRa presets.
    #[test]
    fn routing_app_packet_emits_failed_ack() {
        use crate::meshtastic::ack::AckResult;
        use crate::ports::ROUTING_APP;

        let request_id = 12345_u32;
        let routing_payload = Routing {
            variant: Some(routing::Variant::ErrorReason(
                routing::Error::MaxRetransmit as i32,
            )),
        };
        let mut buf = Vec::with_capacity(routing_payload.encoded_len());
        routing_payload.encode(&mut buf).unwrap();

        let pkt = MeshPacket {
            from: 8,
            to: 0xfeed_face,
            id: 99,
            payload_variant: Some(mesh_packet::PayloadVariant::Decoded(crate::proto::Data {
                portnum: ROUTING_APP as i32,
                payload: buf,
                request_id,
                ..Default::default()
            })),
            ..Default::default()
        };
        let bytes = encode(from_radio::PayloadVariant::Packet(pkt));
        let nodes = HashMap::new();
        let ev = decode_inbound(&bytes, &ctx(Some(0xfeed_face), &nodes)).unwrap();
        let ack = ev
            .iter()
            .find_map(|e| match e {
                InboundEvent::AckOrNak { request_id, result } => Some((*request_id, *result)),
                _ => None,
            })
            .expect("expected AckOrNak event");
        assert_eq!(
            ack,
            (request_id, AckResult::Failed(routing::Error::MaxRetransmit)),
        );
    }

    /// Routing packets carrying RouteDiscovery (not an ack) shouldn't
    /// surface as AckOrNak — those are mesh-routing control messages,
    /// not delivery reports. Likewise, packets without a `request_id`
    /// aren't acking anything specific.
    #[test]
    fn routing_app_route_discovery_does_not_emit_ack() {
        use crate::ports::ROUTING_APP;

        // request_id = 0 → not an ack/nak for any of our packets.
        let routing_payload = Routing {
            variant: Some(routing::Variant::ErrorReason(routing::Error::None as i32)),
        };
        let mut buf = Vec::with_capacity(routing_payload.encoded_len());
        routing_payload.encode(&mut buf).unwrap();
        let pkt = MeshPacket {
            payload_variant: Some(mesh_packet::PayloadVariant::Decoded(crate::proto::Data {
                portnum: ROUTING_APP as i32,
                payload: buf,
                request_id: 0,
                ..Default::default()
            })),
            ..Default::default()
        };
        let bytes = encode(from_radio::PayloadVariant::Packet(pkt));
        let nodes = HashMap::new();
        let ev = decode_inbound(&bytes, &ctx(Some(1), &nodes)).unwrap();
        assert!(
            !ev.iter()
                .any(|e| matches!(e, InboundEvent::AckOrNak { .. })),
            "request_id=0 must not produce an AckOrNak event",
        );
    }
}
