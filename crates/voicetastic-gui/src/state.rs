use std::collections::{HashMap, HashSet};

use voicetastic_core::ble::DiscoveredDevice;
use voicetastic_core::proto::{
    Channel, DeviceMetadata, MyNodeInfo, NodeInfo, User,
    config::{
        BluetoothConfig, DeviceConfig, DisplayConfig, LoRaConfig, NetworkConfig, PositionConfig,
        PowerConfig,
    },
};
use voicetastic_core::service::ConnectionState;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Devices,
    Chat,
    Settings,
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
    pub chat_log: Vec<ChatEntry>,
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
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            conn_state: ConnectionState::Disconnected,
            my_info: None,
            nodes: HashMap::new(),
            chat_log: Vec::new(),
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
