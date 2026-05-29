//! Pure-Rust Opus decoder (decode-only — no encode here).
//!
//! Wraps the `opus-decoder` crate (RFC 8251, no `unsafe`, no FFI) into the
//! same shape as [`super::c2`]: one-shot `opus_decode(payload, codec_param)
//! -> (Vec<f32>, sample_rate)`. Used by the browser client so it can play
//! Opus voice messages from desktop/Android without dragging in `audiopus`'s
//! C/CMake build (which doesn't compile for wasm).
//!
//! Wire format mirrors [`super::imp`]'s Opus path: concatenated
//! `[u16 BE length][opus packet]` chunks, each packet 20 ms of mono audio at
//! 48 kHz. `codec_param` (the desktop encoder's bitrate hint) is unused —
//! Opus packets are self-describing on decode.

use opus_decoder::OpusDecoder;

use super::SAMPLE_RATE_HZ;
use super::error::CodecError;

/// Decode the length-prefixed Opus payload to 48 kHz mono f32 PCM.
/// Returns `(pcm, sample_rate_hz)`.
pub fn opus_decode(payload: &[u8], _codec_param: u8) -> Result<(Vec<f32>, u32), CodecError> {
    let mut decoder = OpusDecoder::new(SAMPLE_RATE_HZ, 1)
        .map_err(|e| CodecError::Codec(format!("opus init: {e:?}")))?;
    // Max frame size per channel @ 48 kHz (120 ms — covers any legal Opus frame).
    let mut scratch = vec![0.0f32; OpusDecoder::MAX_FRAME_SIZE_48K];
    let mut out: Vec<f32> = Vec::new();

    let mut i = 0;
    while i + 2 <= payload.len() {
        let len = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
        i += 2;
        if i + len > payload.len() {
            // Truncated payload — return what we have.
            break;
        }
        let packet = &payload[i..i + len];
        i += len;
        let samples = decoder
            .decode_float(packet, &mut scratch, false)
            .map_err(|e| CodecError::Codec(format!("opus decode: {e:?}")))?;
        out.extend_from_slice(&scratch[..samples]);
    }
    Ok((out, SAMPLE_RATE_HZ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_payload_is_empty_pcm() {
        let (pcm, rate) = opus_decode(&[], 0).expect("decode");
        assert_eq!(rate, SAMPLE_RATE_HZ);
        assert!(pcm.is_empty());
    }

    #[test]
    fn truncated_payload_does_not_panic() {
        // Length header claims more bytes than follow.
        let mut bad = vec![0x00, 0xFF];
        bad.extend_from_slice(&[0u8; 4]);
        let (pcm, _) = opus_decode(&bad, 0).expect("decode");
        assert!(pcm.is_empty());
    }
}
