//! Browser-side Opus encode + decode via the vendored libopus compiled to
//! a standalone wasm by the `libopus` sub-crate.
//!
//! Wire-compatible with the native path in [`super::imp`]'s Opus arm — both
//! delegate to the same libopus code, just via different bindings (native
//! FFI via `audiopus` on desktop, a standalone wasm + a JS shim here).
//! 48 kHz mono f32 PCM in / out, resampled in/out of native rates by
//! [`super::resampler`].
//!
//! Replaces the pure-Rust [`super::opus_d`] decode path when the
//! `opus-wasm` feature is enabled — same wire format, same sample rate,
//! plus encode capability. Keep `opus-decode` enabled if you want
//! decode-only without the encoder weight.
//!
//! Built FIXED_POINT, so the wasm boundary is i16; the Rust wrapper
//! converts f32 PCM ↔ i16 around each call.

use wasm_bindgen::prelude::*;

use super::SAMPLE_RATE_HZ;
use super::resampler::Resampler;

/// Samples per Opus frame on the wire (20 ms @ 48 kHz mono).
const FRAME_SAMPLES: usize = 960;

/// Default Opus bitrate (kbps) when `codec_param` is 0. Matches the native
/// path's fallback in [`super::imp`] and the `OPUS_BITRATE` constant.
const DEFAULT_BITRATE_KBPS: u32 = 12;

#[wasm_bindgen(module = "/src/codec/opus_shim.js")]
extern "C" {
    /// Hand the standalone-wasm bytes to the JS shim. Idempotent in effect:
    /// subsequent calls are ignored once the first instantiation succeeds.
    #[wasm_bindgen(js_name = opusProvideBytes, catch)]
    async fn opus_provide_bytes(bytes: &[u8]) -> Result<JsValue, JsValue>;

    /// Encode a clip of i16 PCM at 48 kHz to length-prefixed Opus packets
    /// (`[u16 BE length][packet bytes]...`).
    #[wasm_bindgen(js_name = opusEncodeClip, catch)]
    async fn opus_encode_clip_js(speech: &[i16], bitrate_bps: u32) -> Result<JsValue, JsValue>;

    /// Decode length-prefixed Opus packets to i16 PCM at 48 kHz.
    #[wasm_bindgen(js_name = opusDecodeClip, catch)]
    async fn opus_decode_clip_js(payload: &[u8]) -> Result<JsValue, JsValue>;
}

/// Initialise the Opus shim with the standalone wasm bytes baked into the
/// `libopus` crate. Safe to call multiple times — the JS side keeps a
/// single `Promise` it resolves on the first successful instance.
pub async fn init() -> Result<(), JsValue> {
    opus_provide_bytes(libopus::wasm_module_bytes()).await?;
    Ok(())
}

/// Encode 48 kHz mono f32 PCM to length-prefixed Opus packets.
///
/// `codec_param` is the target bitrate in kbps (matches the native encoder's
/// argument). `0` falls back to [`DEFAULT_BITRATE_KBPS`]. Trailing samples
/// shorter than one frame (< 20 ms @ 48 kHz) are dropped — same as the
/// native Opus path.
pub async fn opus_encode(pcm: &[f32], in_rate: u32, codec_param: u8) -> Result<Vec<u8>, JsValue> {
    let mut pcm48k = Vec::with_capacity(pcm.len());
    Resampler::new(in_rate, SAMPLE_RATE_HZ).push(pcm, &mut pcm48k);
    let usable = (pcm48k.len() / FRAME_SAMPLES) * FRAME_SAMPLES;
    let speech_i16: Vec<i16> = pcm48k[..usable]
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect();
    let kbps = if codec_param == 0 {
        DEFAULT_BITRATE_KBPS
    } else {
        codec_param as u32
    };
    let bitrate_bps = kbps.saturating_mul(1000);
    let js_val = opus_encode_clip_js(&speech_i16, bitrate_bps).await?;
    let arr: js_sys::Uint8Array = js_val.dyn_into()?;
    Ok(arr.to_vec())
}

/// Decode length-prefixed Opus packets to 48 kHz mono f32 PCM.
/// `codec_param` is unused — Opus packets are self-describing on decode.
pub async fn opus_decode(payload: &[u8], _codec_param: u8) -> Result<(Vec<f32>, u32), JsValue> {
    let js_val = opus_decode_clip_js(payload).await?;
    let arr: js_sys::Int16Array = js_val.dyn_into()?;
    let i16s = arr.to_vec();
    let pcm: Vec<f32> = i16s.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
    Ok((pcm, SAMPLE_RATE_HZ))
}
