//! Finalization helpers: producing a [`VoiceMessage`] from an
//! [`AssemblyState`] and managing the recent-completion blacklist.

use std::time::Instant;

use super::super::consts::BLACKLIST_MAX;
use super::super::message::VoiceMessage;
use super::state::AssemblyState;

/// Append `key` to the blacklist, capped at [`BLACKLIST_MAX`]. Idempotent.
pub(super) fn push_blacklist(
    bl: &mut Vec<((String, u32), Instant)>,
    key: (String, u32),
    now: Instant,
) {
    if bl.iter().any(|(k, _)| *k == key) {
        return;
    }
    bl.push((key, now));
    if bl.len() > BLACKLIST_MAX {
        let drop = bl.len() - BLACKLIST_MAX;
        bl.drain(0..drop);
    }
}

/// Convert a (possibly partial) [`AssemblyState`] into a [`VoiceMessage`],
/// padding any missing data shards with zeros so downstream codec decoders
/// see a stable buffer layout.
pub(super) fn finalize(
    from: &str,
    key: &(String, u32),
    state: AssemblyState,
    complete: bool,
) -> VoiceMessage {
    // chunk_size may still be None if we only ever saw the (trimmed) final
    // DATA chunk. Fall back to that body's length for capacity hints / fill.
    let chunk_size = state.chunk_size.unwrap_or_else(|| {
        state
            .data_shards
            .iter()
            .filter_map(|s| s.as_ref().map(|b| b.len()))
            .max()
            .unwrap_or(0)
    });
    let mut audio = Vec::with_capacity(chunk_size * state.data_shards.len());
    for slot in &state.data_shards {
        match slot {
            Some(payload) => audio.extend_from_slice(payload),
            None => {
                // Missing chunk → fill with zeros (codec-specific silence is
                // the responsibility of the decoder/playback layer).
                audio.resize(audio.len() + chunk_size, 0);
            }
        }
    }
    VoiceMessage {
        message_id: key.1,
        from: from.to_string(),
        to: state.to,
        stream_seq: state.header_template.stream_seq,
        codec: state.header_template.codec,
        codec_param: state.header_template.codec_param,
        audio,
        timestamp: state.first_seen,
        is_complete: complete,
        total_data: state.header_template.total_data,
        received_data: state.received_data,
        recovered_via_fec: state.recovered_via_fec,
        channel: state.channel,
        encrypted: state.encrypted_seen,
    }
}
