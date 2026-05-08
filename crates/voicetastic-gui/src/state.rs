use std::collections::HashMap;

use voicetastic_core::ble::DiscoveredDevice;
use voicetastic_core::proto::{MyNodeInfo, NodeInfo};
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

pub struct SharedState {
    pub conn_state: ConnectionState,
    pub my_info: Option<MyNodeInfo>,
    pub nodes: HashMap<u32, NodeInfo>,
    pub chat_log: Vec<ChatEntry>,
    pub scan_results: Vec<DiscoveredDevice>,
    pub scanning: bool,
    pub status_msg: Option<String>,
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
        }
    }
}
