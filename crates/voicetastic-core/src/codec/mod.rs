//! Codec encode/decode for Opus, Codec2, and AMR-NB.
//!
//! The actual codec implementations are gated behind the `codecs` Cargo feature
//! (`opus` and `codec2` crates, plus raw FFI to `libopencore-amrnb`). Without
//! the feature, the public API surface (constants, [`OpusBandwidth`],
//! [`RecordedClip`], [`payload_duration_ms`], [`is_available`]) is still
//! available; calling [`Encoder::new`] or [`decode`] returns
//! [`CodecError::FeatureDisabled`].
//!
//! # Wire formats
//!
//! Per-codec serialisation of [`RecordedClip::payload`]:
//!
//! - **Opus** (`VoiceCodec::Opus`, `codec_param = bitrate_kbps`): a sequence
//!   of length-prefixed packets:
//!
//!   ```text
//!   [u16 BE length][opus packet bytes] [u16 BE length][opus packet bytes] ...
//!   ```
//!
//!   Each packet covers 20 ms of mono audio at 48 kHz, encoded with
//!   `Application::Voip`.
//!
//! - **Codec2** (`VoiceCodec::Codec2`, `codec_param = mode in 0..=5`):
//!   raw concatenated packed frames of the mode's fixed size
//!   (`bits_per_frame / 8`, rounded up). 8 kHz mono internally.
//!
//! - **AMR-NB** (`VoiceCodec::AmrNb`, `codec_param = mode in 0..=7`):
//!   concatenated IF1 storage-format frames. Each frame is a 1-byte
//!   ToC header (encoding the mode in bits 3..6) followed by the
//!   mode-specific number of speech bytes, for totals (incl. ToC) of
//!   13/14/16/18/20/21/27/32 bytes per 20 ms frame. 8 kHz mono internally.
//!   The actual encode/decode work goes through `libopencore-amrnb` over
//!   raw FFI.

pub(crate) mod frames;

mod denoise;
mod error;
mod resampler;

pub use denoise::{DENOISE_FRAME_SIZE, Denoiser, denoise_available};
pub use error::CodecError;
pub use resampler::Resampler;

use crate::voice::VoiceCodec;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Sample rate used for both capture and playback for the Opus path, and
/// the rate the playback pipeline expects after [`decode`].
#[allow(dead_code)]
pub const SAMPLE_RATE_HZ: u32 = 48_000;
/// Mono frame size (samples) corresponding to a 20 ms Opus packet at 48 kHz.
#[allow(dead_code)]
pub const FRAME_SAMPLES: usize = 960;
/// Sample rate Codec2 operates on (all modes).
#[allow(dead_code)]
pub const CODEC2_SAMPLE_RATE_HZ: u32 = 8_000;
/// Sample rate AMR-NB operates on (all modes).
#[allow(dead_code)]
pub const AMRNB_SAMPLE_RATE_HZ: u32 = 8_000;
/// Samples per AMR-NB frame (20 ms @ 8 kHz mono).
#[allow(dead_code)]
pub const AMRNB_SAMPLES_PER_FRAME: usize = 160;
/// Default Opus bitrate, in bps. Used as fallback when `codec_param == 0`.
#[allow(dead_code)]
pub const OPUS_BITRATE: i32 = 12_000;

// ---------------------------------------------------------------------------
// OpusBandwidth
// ---------------------------------------------------------------------------

/// Sender-side Opus bandwidth selector. Receivers don't need this — the Opus
/// bitstream self-describes per packet.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OpusBandwidth {
    /// SILK 8 kHz — telephony-grade voice, lowest airtime.
    Narrow,
    /// SILK 16 kHz — HD voice, default.
    #[default]
    Wide,
}

// ---------------------------------------------------------------------------
// RecordedClip
// ---------------------------------------------------------------------------

/// A finished recording, ready to feed to the voice protocol.
#[derive(Debug, Clone)]
pub struct RecordedClip {
    /// Encoded codec payload — see module docs for per-codec layout.
    pub payload: Vec<u8>,
    /// Codec identifier matching `voice::VoiceCodec`.
    pub codec: VoiceCodec,
    /// Codec-specific parameter byte (e.g. Codec2 mode index).
    pub codec_param: u8,
    /// Wall-clock duration of the original audio.
    pub duration: std::time::Duration,
}

// ---------------------------------------------------------------------------
// Feature check
// ---------------------------------------------------------------------------

/// `true` when the library was built with `--features codecs`.
pub const fn is_available() -> bool {
    cfg!(feature = "codecs")
}

// ---------------------------------------------------------------------------
// payload_duration_ms
// ---------------------------------------------------------------------------

/// Codec2 samples per encoded frame for each mode index `0..=5`.
const CODEC2_SAMPLES_PER_FRAME: [usize; 6] = [160, 160, 320, 320, 320, 320];
/// Packed bytes per Codec2 frame for each mode index `0..=5`.
const CODEC2_BYTES_PER_FRAME: [usize; 6] = [8, 6, 8, 7, 7, 6];

fn codec2_frame_sizes(mode: u8) -> Option<(usize, usize)> {
    let i = mode as usize;
    if i < CODEC2_SAMPLES_PER_FRAME.len() {
        Some((CODEC2_SAMPLES_PER_FRAME[i], CODEC2_BYTES_PER_FRAME[i]))
    } else {
        None
    }
}

/// Best-effort estimate of the wall-clock duration of an encoded payload,
/// in milliseconds. Returns 0 for unknown codec parameters.
///
/// Opus duration is derived from the TOC byte of each packet (RFC 6716 §3.1)
/// rather than assuming 20 ms per packet, so non-standard frame sizes are
/// handled correctly. AMR-NB correctly counts SID and NO_DATA frames as 20 ms
/// each (they produce one 160-sample block through the decoder).
pub fn payload_duration_ms(payload: &[u8], codec: VoiceCodec, codec_param: u8) -> u32 {
    match codec {
        VoiceCodec::Opus => {
            let mut total_samples: u64 = 0;
            for pkt in frames::OpusPackets::new(payload) {
                total_samples += frames::opus_packet_samples_48k(pkt).unwrap_or(960) as u64;
            }
            (total_samples / 48) as u32
        }
        VoiceCodec::Codec2 => {
            let Some((samples, bytes)) = codec2_frame_sizes(codec_param) else {
                return 0;
            };
            let frame_count = (payload.len() / bytes) as u32;
            frame_count * (samples as u32) / 8
        }
        VoiceCodec::AmrNb => {
            // Each AMR-NB frame (speech, SID, or NO_DATA) covers 20 ms.
            frames::AmrnbFrames::new(payload).count() as u32 * 20
        }
        _ => 0,
    }
}

/// Like [`payload_duration_ms`] but correct for partial payloads with zero-
/// padded gaps. The total duration is independent of which chunks arrived
/// (silence still occupies its slot on the timeline), so this only changes
/// how frames are *counted*, never the result for a complete payload.
///
/// For AMR-NB the per-frame walker would misread zero-padded gap bytes as
/// 13-byte mode-0 frames; since the encoder runs a fixed mode (DTX off) every
/// frame is the same size, so we count `len / frame_size(codec_param)` instead.
/// Other codecs delegate to [`payload_duration_ms`] (Codec2 already derives
/// the count from the fixed frame size; Opus is best-effort / deprecated).
pub fn payload_duration_ms_with_gaps(
    payload: &[u8],
    gaps: &[std::ops::Range<usize>],
    codec: VoiceCodec,
    codec_param: u8,
) -> u32 {
    if gaps.is_empty() {
        return payload_duration_ms(payload, codec, codec_param);
    }
    match codec {
        VoiceCodec::AmrNb => {
            match frames::AMRNB_FRAME_BYTES
                .get(codec_param as usize)
                .copied()
                .flatten()
            {
                Some(frame_bytes) if frame_bytes > 0 => (payload.len() / frame_bytes) as u32 * 20,
                _ => payload_duration_ms(payload, codec, codec_param),
            }
        }
        _ => payload_duration_ms(payload, codec, codec_param),
    }
}

// ---------------------------------------------------------------------------
// Feature-gated implementation
// ---------------------------------------------------------------------------

#[cfg(feature = "codecs")]
mod imp;
#[cfg(feature = "codecs")]
pub use imp::{Encoder, decode, decode_with_gaps};

// One-shot Codec2 (pure Rust, wasm-safe). Available whenever `codec2` is on —
// which `codecs` implies, and which wasm consumers enable on its own.
#[cfg(feature = "codec2")]
mod c2;
#[cfg(feature = "codec2")]
pub use c2::{codec2_decode, codec2_encode};

// Pure-Rust Opus *decoder* (no encode — audiopus stays the native encode
// path). Builds for wasm32 so the browser can play back Opus voice from
// desktop/Android.
#[cfg(feature = "opus-decode")]
mod opus_d;
#[cfg(feature = "opus-decode")]
pub use opus_d::opus_decode;

// AMR-NB encode/decode through opencore-amr compiled to a standalone wasm
// (via emscripten — see the `opencore-amrnb` sub-crate). Routes through
// a small JS shim because the standalone wasm runs in its own linear
// memory, distinct from the main Rust wasm. Wire-compatible with the
// native AMR-NB path; `feature = "amrnb-wasm"` ⇒ wasm32 only.
#[cfg(all(feature = "amrnb-wasm", target_arch = "wasm32"))]
mod amrnb_wasm;
#[cfg(all(feature = "amrnb-wasm", target_arch = "wasm32"))]
pub use amrnb_wasm::{amrnb_decode, amrnb_encode, init as amrnb_init};

// Opus encode + decode through libopus compiled to a standalone wasm (via
// emscripten — see the `libopus` sub-crate). Same JS-shim pattern as
// AMR-NB. Wire-compatible with the native Opus path (audiopus); `feature
// = "opus-wasm"` ⇒ wasm32 only. Supersedes `opus-decode` by adding encode
// and matching the C reference bit-for-bit.
#[cfg(all(feature = "opus-wasm", target_arch = "wasm32"))]
mod opus_wasm;
#[cfg(all(feature = "opus-wasm", target_arch = "wasm32"))]
pub use opus_wasm::{init as opus_init, opus_decode as opus_wasm_decode, opus_encode};

#[cfg(not(feature = "codecs"))]
mod disabled {
    use super::*;

    pub struct Encoder;

    impl Encoder {
        pub fn new(
            _codec_id: VoiceCodec,
            _codec_param: u8,
            _opus_bw: OpusBandwidth,
        ) -> Result<Self, CodecError> {
            Err(CodecError::FeatureDisabled)
        }

        pub fn push(&mut self, _src: &[f32]) -> Result<(), CodecError> {
            Err(CodecError::FeatureDisabled)
        }

        pub fn finish(self) -> Result<Vec<u8>, CodecError> {
            Err(CodecError::FeatureDisabled)
        }
    }

    pub fn decode(
        _payload: &[u8],
        _codec_id: VoiceCodec,
        _codec_param: u8,
    ) -> Result<Vec<i16>, CodecError> {
        Err(CodecError::FeatureDisabled)
    }

    pub fn decode_with_gaps(
        _payload: &[u8],
        _gaps: &[std::ops::Range<usize>],
        _codec_id: VoiceCodec,
        _codec_param: u8,
    ) -> Result<Vec<i16>, CodecError> {
        Err(CodecError::FeatureDisabled)
    }
}
#[cfg(not(feature = "codecs"))]
pub use disabled::{Encoder, decode, decode_with_gaps};
