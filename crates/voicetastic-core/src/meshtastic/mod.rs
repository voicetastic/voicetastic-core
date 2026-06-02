//! Meshtastic-specific implementation.
//!
//! Top-level entry point is [`service::MeshtasticService`]; voice protocol
//! primitives live in [`crate::voice`].

#[cfg(feature = "ble-btleplug")]
pub mod ble;
#[cfg(feature = "serial-tokio")]
pub mod serial;

pub mod ack;
pub mod ids;
pub mod pkc;
pub mod ports;
pub mod proto;
pub mod reconnect;

#[cfg(all(feature = "ble-btleplug", target_os = "linux"))]
pub mod pairing;

pub mod service;

pub use service::MeshtasticService;
