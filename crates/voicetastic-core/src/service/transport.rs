//! Internal abstraction over BLE vs serial transports.

use std::sync::Arc;

use crate::ble::Connection;
use crate::error::Result;
use crate::serial::SerialConnection;

/// Abstraction over BLE vs serial transport.
pub(super) enum Transport {
    Ble(Arc<Connection>),
    Serial(Arc<SerialConnection>),
}

impl Transport {
    pub(super) async fn write_to_radio(&self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Ble(c) => c.write_to_radio(bytes).await,
            Self::Serial(c) => c.write_to_radio(bytes).await,
        }
    }

    pub(super) async fn disconnect(&self) -> Result<()> {
        match self {
            Self::Ble(c) => c.disconnect().await,
            Self::Serial(c) => c.disconnect().await,
        }
    }
}
