//! `voicetastic-proto`: the `no_std` wire-protocol core shared by every client.
//!
//! This is the single, normative implementation of the Voicetastic voice wire
//! format - header + MAC, packet/codec types, the chunker, and Reed-Solomon
//! FEC - extracted so it can run everywhere, including bare-metal ESP32-S3
//! firmware (`xtensa-esp32s3-none-elf`, `-Zbuild-std=core,alloc`). It carries
//! no transport, no async runtime, and no codec/denoiser (those stay
//! host-side: a C `libcodec2` on firmware, the Rust codecs on std hosts).
//!
//! `voicetastic-core` depends on this crate and re-exports it, so std clients
//! (CLI/GUI/web/Android) keep their existing `voicetastic_core::voice::*` and
//! `voicetastic_core::node` paths - one implementation, many drivers.
//!
//! `no_std` except under `cfg(test)`, where the std test harness is linked.
#![cfg_attr(not(test), no_std)]

#[macro_use]
extern crate alloc;

pub mod consts;
pub mod types;
pub mod error;
pub mod mac;
pub mod header;
pub mod builder;
