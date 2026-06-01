//! Transport abstraction over which [`crate::service::MeshService`] exchanges
//! raw `ToRadio` / `FromRadio` byte frames with a Meshtastic device.
//!
//! Built-in implementations live in:
//! - [`crate::ble::Connection`] (feature `ble-btleplug`, default on)
//! - [`crate::serial::SerialConnection`] (feature `serial-tokio`, default on)
//!
//! Downstream consumers can implement this trait themselves to plug
//! `MeshService` into a different transport — e.g. the Android BLE/USB
//! stacks exposed via JNI, an in-process loopback for tests, or a TCP
//! bridge to a remote radio.
//!
//! # Inbound stream
//!
//! `Transport` only models the **outbound** half (`write_to_radio`) and
//! lifecycle (`disconnect`). Inbound `FromRadio` frames are delivered
//! out-of-band via the [`tokio::sync::mpsc::Receiver<Vec<u8>>`] passed to
//! [`crate::service::MeshService::connect_with_transport`]. This split
//! matches how the built-in transports already work (BLE notifications and
//! the serial reader task push into an mpsc queue) and keeps the trait
//! `dyn`-compatible without forcing implementers to expose a stream type.

use async_trait::async_trait;

use crate::error::Result;

/// Bidirectional byte-frame transport to a Meshtastic radio.
///
/// Each call to [`write_to_radio`](Self::write_to_radio) takes a single
/// already-encoded `ToRadio` protobuf message; framing (BLE GATT writes,
/// COBS over serial, …) is the implementer's responsibility.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send one already-encoded `ToRadio` protobuf message.
    async fn write_to_radio(&self, bytes: &[u8]) -> Result<()>;

    /// Tear down the underlying connection. Called by
    /// [`crate::service::MeshService::disconnect`] and on inbound stream
    /// EOF. Implementations should be idempotent.
    async fn disconnect(&self) -> Result<()>;

    /// Maximum bytes per single [`write_to_radio`](Self::write_to_radio)
    /// call that the transport can carry without truncation.
    ///
    /// Returns [`usize::MAX`] by default for transports with no fixed
    /// per-write cap (USB serial, in-process loopback). BLE transports
    /// should override to return `negotiated_ATT_MTU − 3`: GATT
    /// characteristic writes are hard-capped at that size and any
    /// excess is silently dropped by the controller. Most Meshtastic
    /// firmwares cap the negotiated MTU at 255, leaving 252 bytes per
    /// write; the firmware's BLE stack does NOT implement GATT
    /// Long Write (Prepare/Execute), so going past this cap is a
    /// guaranteed silent drop.
    ///
    /// Used by [`crate::voice::sender::VoiceSender`] (through
    /// [`crate::MeshtasticService::max_voice_body_size`]) to size voice
    /// chunks so each wire frame fits in a single transport write.
    fn max_tx_payload(&self) -> usize {
        usize::MAX
    }
}
