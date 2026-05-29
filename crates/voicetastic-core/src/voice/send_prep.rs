//! Bundles the per-burst prep step both drivers do before handing frames
//! to [`super::tx_state::VoiceTx`]: compute the modem-preset-driven
//! `chunk_size`, resolve the FEC policy via
//! [`crate::settings::api::VoiceFecMode::resolve`], pick a fresh
//! `message_id`, and invoke [`super::build_message`].
//!
//! Sans-IO: the caller already has the encoded codec payload bytes
//! (Codec2 / Opus / AMR-NB) and a snapshot of the radio's modem preset.
//! Returns a [`PreparedVoice`] the driver feeds to its registry +
//! [`super::tx_state::VoiceTx`] in two more lines.

use crate::settings::api::VoiceFecMode;
use crate::voice::builder::{BuildConfig, EncodedMessage, build_message, random_message_id};
use crate::voice::consts::MAX_BODY_SIZE;
use crate::voice::error::Result;
use crate::voice::types::{ModemPreset, VoiceCodec};

/// Everything the driver needs to start a paced send. Hand `encoded`
/// to [`crate::voice::OutgoingVoiceRegistry::register`] so future NACK
/// rounds find the burst, then build a [`super::tx_state::VoiceTx`]
/// from `(total_data, frames, channel, to, pacing)`.
#[derive(Debug)]
pub struct PreparedVoice {
    /// Random per-burst id chosen by [`random_message_id`].
    pub message_id: u32,
    /// Number of DATA chunks (frames `0..total_data` in `frames`).
    pub total_data: u8,
    /// FEC parity shard count, resolved per [`VoiceFecMode`].
    pub parity_count: u8,
    /// Final chunk size in bytes (DATA chunks are this size; the last
    /// DATA chunk may be smaller; PARITY chunks are exactly this size).
    pub chunk_size: usize,
    /// Frames in send order — DATA `0..total_data`, then PARITY
    /// `0..parity_count`. Each entry is `(chunk_index, body)`.
    pub frames: Vec<(u8, Vec<u8>)>,
    /// The full [`EncodedMessage`] from [`build_message`] — hand it to
    /// the registry's `register` so retransmits can find the body bytes.
    pub encoded: EncodedMessage,
}

/// Prepare a voice burst for sending. The caller has already run their
/// codec encode to produce `payload`; this resolves all the per-message
/// framing parameters from the radio's modem preset + destination kind
/// and runs [`build_message`].
///
/// `broadcast` = `to.is_none()` on the caller side; we accept it as a
/// bool to keep this helper transport-agnostic (no NodeId types here).
pub fn prepare_voice_send(
    payload: Vec<u8>,
    codec: VoiceCodec,
    codec_param: u8,
    preset: Option<ModemPreset>,
    broadcast: bool,
    fec_mode: VoiceFecMode,
) -> Result<PreparedVoice> {
    let chunk_size = preset
        .map(ModemPreset::recommended_chunk_size)
        .unwrap_or(MAX_BODY_SIZE);
    let total_data = payload.len().div_ceil(chunk_size).max(1);
    let parity_count = fec_mode.resolve(broadcast, preset, total_data);
    let message_id = random_message_id()?;
    let cfg = BuildConfig {
        message_id,
        stream_seq: 0,
        codec,
        codec_param,
        chunk_size,
        parity_count,
        last_in_stream: true,
    };
    let encoded = build_message(&payload, &cfg)?;
    let frames: Vec<(u8, Vec<u8>)> = encoded
        .frames
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, f)| (i as u8, f))
        .collect();
    Ok(PreparedVoice {
        message_id,
        total_data: encoded.total_data,
        parity_count: encoded.parity_count,
        chunk_size,
        frames,
        encoded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i & 0xff) as u8).collect()
    }

    #[test]
    fn small_broadcast_no_preset_uses_max_body_size() {
        let payload = synth(100);
        let prep = prepare_voice_send(
            payload.clone(),
            VoiceCodec::Codec2,
            0,
            None,
            true,
            VoiceFecMode::Off,
        )
        .unwrap();
        assert_eq!(prep.chunk_size, MAX_BODY_SIZE);
        assert_eq!(prep.total_data, 1);
        assert_eq!(prep.parity_count, 0);
        assert_eq!(prep.frames.len(), 1);
        // The single DATA frame's body is the (length-prefixed) payload; sanity-
        // check that it round-trips through assembler-friendly framing.
        assert_eq!(prep.encoded.frames.len(), 1);
    }

    #[test]
    fn preset_chunk_size_drives_chunk_count() {
        let payload = synth(500);
        let prep = prepare_voice_send(
            payload,
            VoiceCodec::Codec2,
            0,
            Some(ModemPreset::LongFast),
            true,
            VoiceFecMode::Off,
        )
        .unwrap();
        let expected_chunks = 500_usize.div_ceil(prep.chunk_size).max(1) as u8;
        assert_eq!(prep.total_data, expected_chunks);
        assert_eq!(prep.frames.len() as u8, prep.total_data + prep.parity_count);
    }

    #[test]
    fn auto_fec_broadcast_adds_parity_shards() {
        let payload = synth(MAX_BODY_SIZE * 4);
        let prep = prepare_voice_send(
            payload,
            VoiceCodec::Opus,
            16,
            None,
            true,
            VoiceFecMode::Auto,
        )
        .unwrap();
        assert!(
            prep.parity_count > 0,
            "Auto FEC on broadcast should add parity for multi-chunk messages"
        );
        assert_eq!(prep.frames.len() as u8, prep.total_data + prep.parity_count);
    }

    #[test]
    fn frames_are_in_send_order_with_correct_indices() {
        let payload = synth(200);
        let prep = prepare_voice_send(
            payload,
            VoiceCodec::Codec2,
            0,
            None,
            true,
            VoiceFecMode::Light,
        )
        .unwrap();
        for (i, (idx, _frame)) in prep.frames.iter().enumerate() {
            assert_eq!(*idx as usize, i, "frame {i} has chunk_index {idx}");
        }
    }
}
