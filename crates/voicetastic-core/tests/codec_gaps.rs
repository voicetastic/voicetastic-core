//! Gap-aware decode (`codec::decode_with_gaps`) for partial voice messages.
//!
//! These exercise the slimmed partial-playback path (Codec2 + AMR-NB only;
//! Opus is deprecated for this protocol and not concealed). They require the
//! native codecs, so the whole file is gated on `--features codecs`.
#![cfg(feature = "codecs")]

use voicetastic_core::codec::{self, OpusBandwidth};
use voicetastic_core::voice::VoiceCodec;

/// ~0.5 s of a 220 Hz tone at 48 kHz mono, the rate the encoders expect.
fn synth_48k(samples: usize) -> Vec<f32> {
    (0..samples)
        .map(|i| {
            let t = i as f32 / 48_000.0;
            (2.0 * std::f32::consts::PI * 220.0 * t).sin() * 0.5
        })
        .collect()
}

fn encode(codec: VoiceCodec, param: u8) -> Vec<u8> {
    let mut enc = codec::Encoder::new(codec, param, OpusBandwidth::Wide).expect("encoder");
    enc.push(&synth_48k(24_000)).expect("push");
    enc.finish().expect("finish")
}

/// With an empty gap list, AMR-NB decodes byte-for-byte identically to the
/// plain `decode` path (its speech decoder is deterministic).
#[test]
fn decode_with_gaps_empty_matches_decode_amrnb() {
    let (codec, param) = (VoiceCodec::AmrNb, 5u8);
    let payload = encode(codec, param);
    let plain = codec::decode(&payload, codec, param).expect("decode");
    let viagap = codec::decode_with_gaps(&payload, &[], codec, param).expect("decode_with_gaps");
    assert_eq!(plain, viagap, "AMR-NB: empty gaps must equal decode()");
}

/// With an empty gap list, both codecs produce the same output length as the
/// plain `decode` path (Codec2's low-rate decoder randomizes unvoiced phase,
/// so only the length is stable sample-to-sample, not the exact values).
#[test]
fn decode_with_gaps_empty_preserves_length() {
    for (codec, param) in [(VoiceCodec::Codec2, 5u8), (VoiceCodec::AmrNb, 5u8)] {
        let payload = encode(codec, param);
        let plain = codec::decode(&payload, codec, param).expect("decode");
        let viagap =
            codec::decode_with_gaps(&payload, &[], codec, param).expect("decode_with_gaps");
        assert_eq!(
            plain.len(),
            viagap.len(),
            "{codec:?}: empty gaps must match decode() length"
        );
    }
}

/// A gap covering the entire Codec2 payload yields pure silence, same length.
#[test]
fn codec2_full_gap_is_silence_same_length() {
    let param = 5u8;
    let payload = encode(VoiceCodec::Codec2, param);
    let baseline = codec::decode(&payload, VoiceCodec::Codec2, param).expect("decode");

    let gaps = [0..payload.len()];
    let out = codec::decode_with_gaps(&payload, &gaps, VoiceCodec::Codec2, param).expect("gaps");

    assert_eq!(out.len(), baseline.len(), "timing (length) preserved");
    assert!(
        out.iter().all(|&s| s == 0),
        "fully-missing Codec2 clip decodes to true silence"
    );
}

/// A mid-payload Codec2 gap is concealed as silence over the missing span
/// while keeping the present audio (real signal energy survives) and the
/// overall length intact.
#[test]
fn codec2_partial_gap_is_silent_over_missing_span() {
    let param = 5u8;
    let payload = encode(VoiceCodec::Codec2, param);
    let baseline = codec::decode(&payload, VoiceCodec::Codec2, param).expect("decode");

    // Gap over the second half; the first half stays present.
    let gap_start = payload.len() / 2;
    let gaps = [gap_start..payload.len()];
    let out = codec::decode_with_gaps(&payload, &gaps, VoiceCodec::Codec2, param).expect("gaps");

    assert_eq!(out.len(), baseline.len(), "length/timing preserved");

    // The tail is deep inside the missing span (every frame there is fully in
    // the gap), so it must be true silence.
    let tail = &out[out.len().saturating_sub(1000)..];
    assert!(
        tail.iter().all(|&s| s == 0),
        "missing-span audio is concealed as silence"
    );

    // The present first half still carries real signal (concealment didn't
    // wipe the received audio).
    let head_peak = out[..out.len() / 4]
        .iter()
        .map(|s| s.unsigned_abs() as u32)
        .max()
        .unwrap_or(0);
    assert!(head_peak > 0, "received audio survives concealment");
}

/// AMR-NB concealment preserves playback length (one NO_DATA frame per lost
/// frame), and a fully-missing clip is low energy (PLC, not garbage).
#[test]
fn amrnb_gap_preserves_length() {
    let param = 5u8;
    let payload = encode(VoiceCodec::AmrNb, param);
    let baseline = codec::decode(&payload, VoiceCodec::AmrNb, param).expect("decode");

    let gaps = [0..payload.len()];
    let out = codec::decode_with_gaps(&payload, &gaps, VoiceCodec::AmrNb, param).expect("gaps");

    assert_eq!(out.len(), baseline.len(), "timing (length) preserved");

    // Concealment of an entirely-lost clip must not synthesize loud audio.
    let peak = out
        .iter()
        .map(|s| s.unsigned_abs() as u32)
        .max()
        .unwrap_or(0);
    let baseline_peak = baseline
        .iter()
        .map(|s| s.unsigned_abs() as u32)
        .max()
        .unwrap_or(0);
    assert!(
        peak <= baseline_peak,
        "concealed peak {peak} should not exceed the real signal peak {baseline_peak}"
    );
}
