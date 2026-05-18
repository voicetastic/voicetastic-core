//! Meshtastic-specific implementation.
//!
//! This module contains all Meshtastic protocol-specific code. Users should depend
//! only on the protocol-agnostic traits in `crate::radio_service` and the voice
//! pipeline in `crate::voice`.

#[cfg(feature = "ble-btleplug")]
pub mod ble;
#[cfg(feature = "serial-tokio")]
pub mod serial;

pub mod ids;
pub mod ports;
pub mod proto;

#[cfg(all(feature = "ble-btleplug", target_os = "linux"))]
pub mod pairing;

pub mod service;

pub use service::MeshtasticService;
