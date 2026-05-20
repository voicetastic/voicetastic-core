//! Finalization helpers: producing a [`VoiceMessage`] from an
//! [`AssemblyState`] and managing the recent-completion blacklist.

use std::time::Instant;

use super::super::consts::BLACKLIST_MAX;
use super::super::message::VoiceMessage;
use super::state::{AssemblyState, SenderKey};

/// Append `key` to the blacklist, capped at [`BLACKLIST_MAX`]. Idempotent.
pub(super) fn push_blacklist(bl: &mut Vec<(SenderKey, Instant)>, key: SenderKey, now: Instant) {
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
    key: &SenderKey,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn push_blacklist_adds_entry() {
        let mut bl = Vec::new();
        let now = std::time::Instant::now();
        let key = (Arc::from("sender1"), 123u32);

        push_blacklist(&mut bl, key.clone(), now);
        assert_eq!(bl.len(), 1);
        assert_eq!(bl[0].0, key);
    }

    #[test]
    fn push_blacklist_idempotent() {
        let mut bl = Vec::new();
        let now = std::time::Instant::now();
        let key = (Arc::from("sender1"), 123u32);

        push_blacklist(&mut bl, key.clone(), now);
        push_blacklist(&mut bl, key.clone(), now);
        assert_eq!(bl.len(), 1, "should not add duplicate");
    }

    #[test]
    fn push_blacklist_respects_max() {
        let mut bl = Vec::new();
        let now = std::time::Instant::now();

        for i in 0..=(BLACKLIST_MAX + 10) {
            let key = (Arc::from(format!("sender{}", i).as_str()), i as u32);
            push_blacklist(&mut bl, key, now);
        }

        assert_eq!(bl.len(), BLACKLIST_MAX, "should not exceed max");
    }

    #[test]
    fn push_blacklist_evicts_oldest() {
        let mut bl = Vec::new();
        let mut time = std::time::Instant::now();

        for i in 0..BLACKLIST_MAX {
            let key = (Arc::from(format!("sender{}", i).as_str()), i as u32);
            push_blacklist(&mut bl, key, time);
            time += std::time::Duration::from_secs(1);
        }

        let oldest_key = bl[0].0.clone();

        let new_key = (Arc::from("senderNew"), 999u32);
        push_blacklist(
            &mut bl,
            new_key.clone(),
            time + std::time::Duration::from_secs(1),
        );

        assert!(
            !bl.iter().any(|(k, _)| k == &oldest_key),
            "oldest should be evicted"
        );
        assert!(
            bl.iter().any(|(k, _)| k == &new_key),
            "new entry should be present"
        );
    }

    #[test]
    fn finalize_complete_message() {
        let chunk_size = 64;
        let codec = super::super::super::types::VoiceCodec::AmrNb;
        let destination = super::super::super::types::VoiceDestination::Broadcast;

        let mut state = AssemblyState::new(
            super::super::super::header::ChunkHeader {
                packet_type: super::super::super::types::PacketType::Data,
                last_in_stream: false,
                message_id: 12345,
                codec,
                codec_param: 5,
                stream_seq: 0,
                chunk_index: 0,
                total_data: 2,
                parity_count: 0,
            },
            Some(chunk_size),
            destination,
            0,
        );

        state.data_shards[0] = Some(vec![1u8; chunk_size]);
        state.data_shards[1] = Some(vec![2u8; 32]);
        state.received_data = 2;

        let key = (Arc::from("test_sender"), 12345u32);
        let msg = finalize("test_sender", &key, state, true);

        assert!(msg.is_complete);
        assert_eq!(msg.message_id, 12345);
        assert_eq!(msg.from, "test_sender");
        assert_eq!(msg.total_data, 2);
        assert_eq!(msg.received_data, 2);
        assert_eq!(msg.codec, codec);
    }

    #[test]
    fn finalize_partial_message_with_padding() {
        let chunk_size = 64;
        let codec = super::super::super::types::VoiceCodec::AmrNb;
        let destination = super::super::super::types::VoiceDestination::Broadcast;

        let mut state = AssemblyState::new(
            super::super::super::header::ChunkHeader {
                packet_type: super::super::super::types::PacketType::Data,
                last_in_stream: false,
                message_id: 12345,
                codec,
                codec_param: 5,
                stream_seq: 0,
                chunk_index: 0,
                total_data: 3,
                parity_count: 0,
            },
            Some(chunk_size),
            destination,
            0,
        );

        state.data_shards[0] = Some(vec![1u8; chunk_size]);
        state.data_shards[1] = None;
        state.data_shards[2] = Some(vec![3u8; 32]);
        state.received_data = 2;

        let key = (Arc::from("test_sender"), 12345u32);
        let msg = finalize("test_sender", &key, state, false);

        assert!(!msg.is_complete);
        assert_eq!(msg.received_data, 2);
        assert_eq!(msg.audio.len(), chunk_size + chunk_size + 32);
        assert_eq!(msg.audio[..chunk_size], vec![1u8; chunk_size][..]);
        assert_eq!(
            msg.audio[chunk_size..chunk_size * 2],
            vec![0u8; chunk_size][..]
        );
        assert_eq!(
            msg.audio[chunk_size * 2..chunk_size * 2 + 32],
            vec![3u8; 32][..]
        );
    }

    #[test]
    fn finalize_with_fec_recovery() {
        let chunk_size = 64;
        let codec = super::super::super::types::VoiceCodec::AmrNb;
        let destination = super::super::super::types::VoiceDestination::Broadcast;

        let mut state = AssemblyState::new(
            super::super::super::header::ChunkHeader {
                packet_type: super::super::super::types::PacketType::Data,
                last_in_stream: false,
                message_id: 12345,
                codec,
                codec_param: 5,
                stream_seq: 0,
                chunk_index: 0,
                total_data: 2,
                parity_count: 0,
            },
            Some(chunk_size),
            destination,
            0,
        );

        state.data_shards[0] = Some(vec![1u8; chunk_size]);
        state.recovered_via_fec = 1;
        state.received_data = 2;

        let key = (Arc::from("test_sender"), 12345u32);
        let msg = finalize("test_sender", &key, state, true);

        assert_eq!(msg.recovered_via_fec, 1);
    }
}
