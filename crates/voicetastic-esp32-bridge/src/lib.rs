//! Minimal C ABI bridge: voicetastic-core -> ESP32 firmware (PlatformIO).
//!
//! Slice 1 of the "firmware builds on core" migration: prove the toolchain
//! (Xtensa cross-compile + static link + FFI call) end to end before moving
//! any protocol logic across. Two entry points: a version string and a Codec2
//! smoke test that forces core's pure-Rust codec2 path to compile + link.
//!
//! The surface intentionally stays tiny here. Once the link is proven on
//! hardware it grows to the sans-IO protocol behind the same ABI:
//! `decode_inbound`, `VoiceAssembler` (reassembly + NACK tick),
//! `OutgoingVoiceRegistry` (retransmit), the chunker/FEC, and the codec -
//! the same logic the web and Android drivers already consume.

use core::ffi::{c_char, c_int};

/// Static, NUL-terminated build identifier. Never null; valid for the whole
/// program lifetime. Lets the firmware log-confirm the core bridge linked.
#[unsafe(no_mangle)]
pub extern "C" fn vt_core_version() -> *const c_char {
    concat!("voicetastic-core-bridge ", env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Smoke test that core's Codec2 encoder links + runs through the bridge:
/// encodes one 40 ms frame of 8 kHz silence at `codec_param` and returns the
/// encoded byte count (> 0 on success, -1 on error). This forces the `codec2`
/// feature to compile + link for the target, not just the trivial string path.
#[unsafe(no_mangle)]
pub extern "C" fn vt_codec2_smoke(codec_param: u8) -> c_int {
    // 320 samples = 40 ms @ 8 kHz: at least one Codec2 frame for any mode.
    let pcm = [0.0f32; 320];
    match voicetastic_core::codec::codec2_encode(&pcm, 8_000, codec_param) {
        Ok(bytes) => bytes.len() as c_int,
        Err(_) => -1,
    }
}
