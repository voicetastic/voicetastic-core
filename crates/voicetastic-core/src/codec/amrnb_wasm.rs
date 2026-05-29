//! Browser-side AMR-NB encode/decode via the vendored opencore-amr-nb
//! compiled to a standalone wasm by the `opencore-amrnb` sub-crate.
//!
//! Wire-compatible with the native path in [`super::imp`]'s AMR-NB arm —
//! both delegate to the same opencore-amr code, just via different bindings
//! (native FFI on desktop, a standalone wasm + a JS shim here). 48 kHz mono
//! f32 PCM in / out, resampled to 8 kHz with the linear resampler in
//! [`super::resampler`].

use wasm_bindgen::prelude::*;

use super::AMRNB_SAMPLE_RATE_HZ;
use super::resampler::Resampler;

/// Samples per AMR-NB frame (20 ms @ 8 kHz).
const FRAME_SAMPLES: usize = 160;

#[wasm_bindgen(module = "/src/codec/amrnb_shim.js")]
extern "C" {
    /// Hand the standalone-wasm bytes to the JS shim. Idempotent in effect:
    /// subsequent calls are ignored once the first instantiation succeeds.
    #[wasm_bindgen(js_name = amrnbProvideBytes, catch)]
    async fn amrnb_provide_bytes(bytes: &[u8]) -> Result<JsValue, JsValue>;

    /// Encode a clip of i16 PCM at 8 kHz to concatenated AMR-NB IF1 frames.
    #[wasm_bindgen(js_name = amrnbEncodeClip, catch)]
    async fn amrnb_encode_clip_js(speech: &[i16], mode: u8) -> Result<JsValue, JsValue>;

    /// Decode concatenated AMR-NB IF1 frames to i16 PCM at 8 kHz.
    #[wasm_bindgen(js_name = amrnbDecodeClip, catch)]
    async fn amrnb_decode_clip_js(payload: &[u8]) -> Result<JsValue, JsValue>;
}

/// Initialise the AMR-NB shim with the standalone wasm bytes baked into the
/// `opencore-amrnb` crate. Safe to call multiple times — the JS side
/// keeps a single `Promise` it resolves on the first successful instance.
pub async fn init() -> Result<(), JsValue> {
    amrnb_provide_bytes(opencore_amrnb::wasm_module_bytes()).await?;
    Ok(())
}

/// Encode 48 kHz mono f32 PCM to concatenated AMR-NB IF1 frames.
///
/// `mode` is the AMR-NB mode index (0..=7). Trailing samples shorter than
/// one frame (< 20 ms @ 8 kHz) are dropped — same as core's Codec2 path.
pub async fn amrnb_encode(pcm: &[f32], in_rate: u32, mode: u8) -> Result<Vec<u8>, JsValue> {
    let mut pcm8k = Vec::with_capacity(pcm.len());
    Resampler::new(in_rate, AMRNB_SAMPLE_RATE_HZ).push(pcm, &mut pcm8k);
    // Trim to a whole number of frames.
    let usable = (pcm8k.len() / FRAME_SAMPLES) * FRAME_SAMPLES;
    let speech_i16: Vec<i16> = pcm8k[..usable]
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect();
    let js_val = amrnb_encode_clip_js(&speech_i16, mode).await?;
    let arr: js_sys::Uint8Array = js_val.dyn_into()?;
    Ok(arr.to_vec())
}

/// Decode AMR-NB IF1 frames to **8 kHz** mono f32 PCM (no upsample — the
/// caller resamples for playback if needed; the chat harness lets Web Audio
/// do it by playing the buffer at 8 kHz directly).
pub async fn amrnb_decode(payload: &[u8]) -> Result<(Vec<f32>, u32), JsValue> {
    let js_val = amrnb_decode_clip_js(payload).await?;
    let arr: js_sys::Int16Array = js_val.dyn_into()?;
    let i16s = arr.to_vec();
    let pcm: Vec<f32> = i16s.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
    Ok((pcm, AMRNB_SAMPLE_RATE_HZ))
}
