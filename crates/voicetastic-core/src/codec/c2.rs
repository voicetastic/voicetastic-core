//! One-shot Codec2 encode/decode.
//!
//! The `codec2` crate is pure Rust, so unlike the full [`super::imp`] path
//! (which pulls Opus's C/CMake bindings) this builds for `wasm32`. It's the
//! Codec2 implementation the browser client uses, keeping the codec in core
//! rather than reimplemented per platform.
//!
//! Wire layout matches [`super::imp`]'s Codec2 path exactly (same
//! `codec_param` → mode mapping, same i16 conversion and frame packing), so
//! audio encoded here interoperates with the desktop/Android clients.

use codec2::{Codec2, Codec2Mode};

use super::CODEC2_SAMPLE_RATE_HZ;
use super::error::CodecError;
use super::resampler::Resampler;

/// Map the on-wire `codec_param` byte to a Codec2 mode (`0..=5`).
fn mode_from_param(b: u8) -> Result<Codec2Mode, CodecError> {
    Ok(match b {
        0 => Codec2Mode::MODE_3200,
        1 => Codec2Mode::MODE_2400,
        2 => Codec2Mode::MODE_1600,
        3 => Codec2Mode::MODE_1400,
        4 => Codec2Mode::MODE_1300,
        5 => Codec2Mode::MODE_1200,
        _ => return Err(CodecError::Codec(format!("unknown codec2 mode index {b}"))),
    })
}

/// Encode mono f32 PCM at `in_rate` Hz into concatenated Codec2 frames (the
/// `audio` payload `voice::build_message` chunks). Resamples to 8 kHz first;
/// trailing samples shorter than one codec frame (< 40 ms) are dropped.
pub fn codec2_encode(pcm: &[f32], in_rate: u32, codec_param: u8) -> Result<Vec<u8>, CodecError> {
    let mut c2 = Codec2::new(mode_from_param(codec_param)?);
    let spf = c2.samples_per_frame();
    let bpf = c2.bits_per_frame().div_ceil(8);

    let mut pcm8k: Vec<f32> = Vec::with_capacity(pcm.len());
    Resampler::new(in_rate, CODEC2_SAMPLE_RATE_HZ).push(pcm, &mut pcm8k);

    let mut payload = Vec::new();
    let mut i = 0;
    while i + spf <= pcm8k.len() {
        let frame_i16: Vec<i16> = pcm8k[i..i + spf]
            .iter()
            .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .collect();
        let mut packed = vec![0u8; bpf];
        c2.encode(&mut packed, &frame_i16);
        payload.extend_from_slice(&packed);
        i += spf;
    }
    Ok(payload)
}

/// Decode concatenated Codec2 frames to **8 kHz** mono f32 PCM (no upsample —
/// the caller resamples on playback). Returns `(pcm, sample_rate_hz)`.
pub fn codec2_decode(payload: &[u8], codec_param: u8) -> Result<(Vec<f32>, u32), CodecError> {
    let mut c2 = Codec2::new(mode_from_param(codec_param)?);
    let spf = c2.samples_per_frame();
    let bpf = c2.bits_per_frame().div_ceil(8);

    let mut pcm: Vec<f32> = Vec::new();
    let mut frame = vec![0i16; spf];
    let mut i = 0;
    while i + bpf <= payload.len() {
        c2.decode(&mut frame, &payload[i..i + bpf]);
        pcm.extend(frame.iter().map(|&s| s as f32 / i16::MAX as f32));
        i += bpf;
    }
    Ok((pcm, CODEC2_SAMPLE_RATE_HZ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_is_plausible() {
        let pcm: Vec<f32> = (0..4000).map(|i| (i as f32 * 0.1).sin() * 0.3).collect();
        let bytes = codec2_encode(&pcm, 8_000, 5).expect("encode");
        assert!(!bytes.is_empty());
        let (out, rate) = codec2_decode(&bytes, 5).expect("decode");
        assert_eq!(rate, 8_000);
        assert!(!out.is_empty());
    }

    #[test]
    fn bad_mode_errors() {
        assert!(codec2_encode(&[0.0; 320], 8_000, 9).is_err());
    }
}
