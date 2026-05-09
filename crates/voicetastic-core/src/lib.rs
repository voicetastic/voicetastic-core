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
//! - [`ble`]: BLE GATT transport implementation (btleplug).
//! - [`service`]: high-level [`MeshService`](service::MeshService) façade.
//! - [`error`]: unified error type.

pub mod error;
pub mod ids;
pub mod ports;
pub mod proto;
pub mod settings;
pub mod voice;

pub mod ble;
pub mod serial;
pub mod service;

pub use error::{Error, Result};
