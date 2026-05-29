use std::collections::{HashMap, HashSet, VecDeque};

use voicetastic_core::ble::DiscoveredDevice;
use voicetastic_core::meshtastic::service::ConnectionState;
#[cfg(target_os = "linux")]
use voicetastic_core::pairing::{PairingPromptKind, PairingResponse};
use voicetastic_core::proto::{
    Channel, DeviceMetadata, MyNodeInfo, NodeInfo, User,
    config::{
        BluetoothConfig, DeviceConfig, DisplayConfig, LoRaConfig, NetworkConfig, PositionConfig,
        PowerConfig,
    },
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Devices,
    Chat,
    Settings,
}

/// Delivery state of an outgoing DM. Surfaced as ✓ / ✓✓ / ❌ / ⏱ in
/// the chat row next to the message. Broadcasts always stay at `None`
/// — the firmware doesn't ack them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStatus {
    /// Sent; waiting for the firmware's `Routing` ack.
    Pending,
    /// Firmware reported `Routing::Error::None`.
    Delivered,
    /// Firmware reported a typed delivery failure.
    Failed,
    /// No ack within the deadline.
    TimedOut,
}

#[derive(Clone)]
pub struct ChatEntry {
    pub text: String,
    #[allow(dead_code)]
    pub rx_time: u32,
    pub outgoing: bool,
    /// Channel index this message belongs to.
    pub channel: u32,
    /// Sender node num (0 for our own outgoing messages where it isn't known yet).
    pub from_num: u32,
    /// Destination node num. `0xFFFF_FFFF` = broadcast.
    pub to_num: u32,
    /// Firmware-reported delivery status for outgoing DMs. `None` for
    /// inbound entries and outgoing broadcasts. The chat UI surfaces
    /// this as a small icon trailing the message text.
    pub delivery: Option<DeliveryStatus>,
    /// Mesh packet id assigned when an outgoing message was sent. Used
    /// by the ack watcher to locate the right `ChatEntry` once the
    /// firmware reports delivery. `None` for inbound entries.
    pub outgoing_packet_id: Option<u32>,
    /// Voice payload (length-prefixed Opus packets) when this entry is a
    /// voice message. `None` for plain text, or for an outgoing voice
    /// entry that hasn't finished sending yet — the payload is attached
    /// once the last chunk leaves the TX queue so the play button
    /// only appears when playback would actually work.
    pub voice: Option<VoicePayload>,
    /// For outgoing voice entries: the protocol `message_id` so we can
    /// upgrade the entry once the send completes (attaching `voice` and
    /// clearing the "sending" label). `None` for text or inbound voice.
    pub outgoing_voice_id: Option<u32>,
    /// For inbound voice entries: the protocol `message_id`. Paired
    /// with `from_num` it lets the assembler watcher locate this entry
    /// to update its "received X/Y chunks" label as data arrives, and
    /// finally promote it to a playable entry on completion. `None`
    /// for text or outgoing voice.
    pub inbound_voice_id: Option<u32>,
}

/// Audio attached to a [`ChatEntry`]. We keep a `codec` byte so future
/// extensions (AMR, raw PCM, etc.) can land without breaking existing
/// entries.
#[derive(Clone)]
pub struct VoicePayload {
    pub codec: voicetastic_core::voice::VoiceCodec,
    /// Codec-specific parameter byte from the wire header (e.g. Codec2
    /// mode index). Required to drive the right decoder on playback.
    pub codec_param: u8,
    pub bytes: Vec<u8>,
    #[allow(dead_code)] // displayed only in tooltips today; surface for future
    pub duration_ms: u32,
}

/// Identifies one editable settings section. Used as a dirty-tracking key
/// so an inbound device push doesn't clobber an in-progress edit.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Owner,
    Lora,
    Device,
    Position,
    Power,
    Network,
    Display,
    Bluetooth,
    Channel(i32),
}

/// In-progress edit of the manually-fixed position. Stored as `f64` degrees
/// for latitude/longitude so the user types human numbers; converted to the
/// proto's `sfixed32 * 1e7` representation on send.
#[derive(Clone, Default)]
pub struct FixedPosEdit {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_m: i32,
}

pub struct SharedState {
    pub conn_state: ConnectionState,
    pub my_info: Option<MyNodeInfo>,
    pub nodes: HashMap<u32, NodeInfo>,
    /// Append-only chat scrollback. Stored as a `VecDeque` so the
    /// FIFO eviction in [`Self::push_chat`] is O(1) `pop_front` rather
    /// than the O(n) shift `Vec::drain(..excess)` paid every time the
    /// log filled up.
    pub chat_log: VecDeque<ChatEntry>,
    pub scan_results: Vec<DiscoveredDevice>,
    pub scanning: bool,
    pub status_msg: Option<String>,

    // Latest snapshots from the device (None until received).
    pub lora: Option<LoRaConfig>,
    pub device: Option<DeviceConfig>,
    pub position: Option<PositionConfig>,
    pub power: Option<PowerConfig>,
    pub network: Option<NetworkConfig>,
    pub display: Option<DisplayConfig>,
    pub bluetooth: Option<BluetoothConfig>,
    pub channels: Vec<Channel>,
    pub owner: Option<User>,
    pub metadata: Option<DeviceMetadata>,

    /// Sections the user has edited locally.
    pub dirty: HashSet<Section>,

    /// Status message specific to the settings tab.
    pub config_status: Option<String>,

    /// Working copy of the manually-fixed position the user is editing.
    /// Seeded lazily from our own NodeInfo's `Position` the first time the
    /// Position section is opened; survives frame-to-frame so the text
    /// fields remember in-progress edits. Coordinates are in degrees.
    pub fixed_pos_edit: Option<FixedPosEdit>,

    /// An in-flight BlueZ pairing prompt waiting for the user to type
    /// the 6-digit passkey shown on the radio's OLED. `None` when no
    /// pairing is in progress.
    #[cfg(target_os = "linux")]
    pub pending_pairing: Option<PendingPairing>,
}

impl Clone for SharedState {
    fn clone(&self) -> Self {
        #[allow(clippy::needless_update)]
        Self {
            conn_state: self.conn_state,
            my_info: self.my_info.clone(),
            nodes: self.nodes.clone(),
            chat_log: self.chat_log.clone(),
            scan_results: self.scan_results.clone(),
            scanning: self.scanning,
            status_msg: self.status_msg.clone(),
            lora: self.lora.clone(),
            device: self.device.clone(),
            position: self.position,
            power: self.power,
            network: self.network.clone(),
            display: self.display,
            bluetooth: self.bluetooth,
            channels: self.channels.clone(),
            owner: self.owner.clone(),
            metadata: self.metadata.clone(),
            dirty: self.dirty.clone(),
            config_status: self.config_status.clone(),
            fixed_pos_edit: self.fixed_pos_edit.clone(),
            #[cfg(target_os = "linux")]
            pending_pairing: None,
        }
    }
}

/// A pairing prompt routed from `org.bluez.Agent1` to the GUI modal.
/// `reply` is consumed when the user clicks OK / Cancel; if the slot is
/// dropped without an explicit reply (e.g. the app exits with the modal
/// open), the `Drop` impl below sends `Cancel` so the agent task on the
/// other end doesn't have to rely on `RecvError::Closed` to infer intent.
#[cfg(target_os = "linux")]
pub struct PendingPairing {
    pub address: String,
    pub kind: PairingPromptKind,
    pub reply: Option<tokio::sync::oneshot::Sender<PairingResponse>>,
    /// In-progress text input for `Passkey` / `PinCode` kinds.
    pub input: String,
}

#[cfg(target_os = "linux")]
impl Drop for PendingPairing {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(PairingResponse::Cancel);
        }
    }
}

/// Maximum number of entries kept in [`SharedState::chat_log`]. Long-running
/// sessions on a busy mesh would otherwise grow this `Vec` without bound and
/// eventually OOM the GUI process. Older entries are dropped FIFO once the
/// log reaches this size.
pub const MAX_CHAT_LOG_ENTRIES: usize = 2000;

impl SharedState {
    /// Append a chat entry and evict the oldest entries if the log exceeds
    /// [`MAX_CHAT_LOG_ENTRIES`]. Callers that mutate existing entries
    /// in-place (e.g. upgrading a "receiving …" placeholder) don't need to
    /// go through this helper.
    pub fn push_chat(&mut self, entry: ChatEntry) {
        self.chat_log.push_back(entry);
        while self.chat_log.len() > MAX_CHAT_LOG_ENTRIES {
            self.chat_log.pop_front();
        }
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            conn_state: ConnectionState::Disconnected,
            my_info: None,
            nodes: HashMap::new(),
            chat_log: VecDeque::new(),
            scan_results: Vec::new(),
            scanning: false,
            status_msg: None,
            lora: None,
            device: None,
            position: None,
            power: None,
            network: None,
            display: None,
            bluetooth: None,
            channels: Vec::new(),
            owner: None,
            metadata: None,
            dirty: HashSet::new(),
            config_status: None,
            fixed_pos_edit: None,
            #[cfg(target_os = "linux")]
            pending_pairing: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mimics the watcher behaviour: only overwrite when the section is not
    /// in the dirty set. This is what `watchers::spawn_watch!` does and what
    /// `settings::card` relies on to preserve in-flight edits.
    fn apply_inbound_lora(
        state: &mut SharedState,
        value: voicetastic_core::proto::config::LoRaConfig,
    ) {
        if !state.dirty.contains(&Section::Lora) {
            state.lora = Some(value);
        }
    }

    #[test]
    fn inbound_does_not_clobber_dirty_section() {
        let mut s = SharedState::default();
        // User starts editing — locally sets a value and marks dirty.
        let edited = voicetastic_core::proto::config::LoRaConfig {
            tx_power: 42,
            ..Default::default()
        };
        s.lora = Some(edited);
        s.dirty.insert(Section::Lora);

        // Device pushes a stale snapshot.
        let from_device = voicetastic_core::proto::config::LoRaConfig {
            tx_power: 0,
            ..Default::default()
        };
        apply_inbound_lora(&mut s, from_device);

        // The local edit must survive.
        assert_eq!(s.lora.as_ref().unwrap().tx_power, 42);
    }

    #[test]
    fn inbound_overwrites_clean_section() {
        let mut s = SharedState::default();
        assert!(s.lora.is_none());
        let from_device = voicetastic_core::proto::config::LoRaConfig {
            tx_power: 7,
            ..Default::default()
        };
        apply_inbound_lora(&mut s, from_device);
        assert_eq!(s.lora.as_ref().unwrap().tx_power, 7);
    }

    #[test]
    fn dirty_set_distinguishes_channels_by_index() {
        let mut s = SharedState::default();
        s.dirty.insert(Section::Channel(0));
        assert!(s.dirty.contains(&Section::Channel(0)));
        assert!(!s.dirty.contains(&Section::Channel(1)));
        assert!(!s.dirty.contains(&Section::Lora));
    }

    #[test]
    fn config_complete_handler_clears_all_dirty() {
        let mut s = SharedState::default();
        s.dirty.insert(Section::Lora);
        s.dirty.insert(Section::Owner);
        s.dirty.insert(Section::Channel(2));
        // Same logic the config_complete watcher runs.
        s.dirty.clear();
        assert!(s.dirty.is_empty());
    }

    #[test]
    fn fixed_pos_edit_round_trip() {
        let mut s = SharedState::default();
        assert!(s.fixed_pos_edit.is_none());
        s.fixed_pos_edit = Some(FixedPosEdit {
            latitude_deg: 48.5,
            longitude_deg: 2.3,
            altitude_m: 120,
        });
        let e = s.fixed_pos_edit.clone().unwrap();
        assert!((e.latitude_deg - 48.5).abs() < 1e-9);
        assert!((e.longitude_deg - 2.3).abs() < 1e-9);
        assert_eq!(e.altitude_m, 120);
    }
}
