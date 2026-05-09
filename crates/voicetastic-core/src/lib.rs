//! Shared core for the Voicetastic desktop client.
//!
//! Wire-compatible with the upstream Android app: same Meshtastic GATT UUIDs,
//! same port numbers, same voice chunk format (protocol version 1).
//!
//! Top-level modules:
//! - [`proto`]: prost-generated Meshtastic protobuf bindings.
//! - [`ids`]: node-number ↔ `!aabbccdd` text-id helpers.
//! - [`ports`]: Meshtastic application port constants.
//! - [`voice`]: AMR-NB chunker / assembler (no codec — bytes only).
//! - [`transport`]: [`Transport`] trait — the seam through which
//!   [`service::MeshService`] talks to a radio. Built-in implementations
//!   ([`ble`], [`serial`]) are feature-gated; downstream consumers
//!   (Android, tests, …) can supply their own.
//! - [`ble`]: BLE GATT transport (feature `ble-btleplug`, default on).
//! - [`serial`]: USB-serial transport (feature `serial-tokio`, default on).
//! - [`service`]: high-level [`MeshService`](service::MeshService) façade.
//! - [`error`]: unified error type.

pub mod error;
pub mod ids;
pub mod ports;
pub mod proto;
pub mod settings;
pub mod transport;
pub mod voice;

#[cfg(feature = "ble-btleplug")]
pub mod ble;
#[cfg(feature = "serial-tokio")]
pub mod serial;
pub mod service;

pub use error::{Error, Result};
pub use transport::Transport;
