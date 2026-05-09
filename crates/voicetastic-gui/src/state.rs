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
    pub from_id: String,
    pub text: String,
    #[allow(dead_code)]
    pub rx_time: u32,
    pub outgoing: bool,
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
