//! Per-message reassembly state and the shared `AssemblerInner` table.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use reed_solomon_erasure::galois_8::ReedSolomon;

use super::super::error::{Result, VoiceError};
use super::super::header::ChunkHeader;
use super::super::types::VoiceDestination;

/// Map key for in-progress assembly entries: `(sender_id, message_id)`.
///
/// Stored as `Arc<str>` rather than `String` so the per-tick key snapshot
/// in [`super::VoiceAssembler::tick`] and the per-frame `push_blacklist` /
/// eviction paths can clone the sender id with a refcount bump instead of
/// a fresh allocation. With many concurrent senders on a busy mesh this
/// is the single hottest allocation in the receive path.
pub(super) type SenderKey = (Arc<str>, u32);

/// One in-progress voice message, keyed by `(from_id, message_id)` in
/// [`AssemblerInner::in_progress`].
pub(super) struct AssemblyState {
    pub(super) header_template: ChunkHeader,
    /// `None` until the first non-final DATA frame or any PARITY frame fixes
    /// the chunk size. A lone trimmed final DATA chunk is not enough.
    pub(super) chunk_size: Option<usize>,
    pub(super) last_data_len: Option<usize>,
    pub(super) data_shards: Vec<Option<Vec<u8>>>,
    pub(super) parity_shards: Vec<Option<Vec<u8>>>,
    pub(super) received_data: u8,
    pub(super) received_parity: u8,
    pub(super) started_at: Instant,
    pub(super) last_chunk_at: Instant,
    /// Last time a real data or parity chunk arrived (never advanced by
    /// NACK emission). Drives dead-sender detection in `tick()`.
    pub(super) last_data_at: Instant,
    pub(super) first_seen: chrono::DateTime<chrono::Utc>,
    /// Number of NACK rounds emitted *since the last accepted shard*. Reset
    /// to 0 in the ingest path whenever real forward progress lands.
    ///
    /// Doubles as the wire-visible `round` field on emitted NACKs **and**
    /// the hard give-up bound: once it reaches `max_nack_rounds` we stop
    /// chasing the message and partial-finalize (or discard).
    ///
    /// Bounding on the consecutive count rather than a cumulative one is
    /// deliberate: a long, slow-trickling message that actually services
    /// every NACK round must not be killed off — only one that has gone
    /// truly silent for `max_nack_rounds * nack_window` should give up.
    /// `message_timeout` remains the absolute upper bound regardless.
    pub(super) nack_rounds: u16,
    /// Count of post-template validation failures (codec / total_data /
    /// stream_seq mismatch). After [`super::config::MAX_VALIDATION_STRIKES`]
    /// the entry is evicted and blacklisted to keep a chatty bad sender
    /// from holding a per-sender slot until the message timeout.
    pub(super) validation_strikes: u8,
    pub(super) to: VoiceDestination,
    pub(super) channel: u32,
    pub(super) encrypted_seen: bool,
    pub(super) recovered_via_fec: u8,
}

impl AssemblyState {
    pub(super) fn new(
        header: ChunkHeader,
        chunk_size: Option<usize>,
        to: VoiceDestination,
        channel: u32,
    ) -> Self {
        Self {
            header_template: header,
            chunk_size,
            last_data_len: None,
            data_shards: vec![None; header.total_data as usize],
            parity_shards: vec![None; header.parity_count as usize],
            received_data: 0,
            received_parity: 0,
            started_at: Instant::now(),
            last_chunk_at: Instant::now(),
            last_data_at: Instant::now(),
            first_seen: chrono::Utc::now(),
            nack_rounds: 0,
            validation_strikes: 0,
            to,
            channel,
            encrypted_seen: false,
            recovered_via_fec: 0,
        }
    }

    /// Attempt Reed–Solomon recovery if we have at least `total_data` shards
    /// in total (data + parity). No-op when `parity_count == 0` or the
    /// chunk size is not yet pinned.
    ///
    /// Note: when the final DATA chunk is the missing shard and its real
    /// (un-padded) length is unknown (we never saw the original frame),
    /// recovery is skipped — reconstructing it from parity would yield a
    /// padded `chunk_size`-byte shard with no way to trim the trailing
    /// zeros, silently corrupting the tail of the audio for many codecs.
    /// The receiver falls back to NACK-driven retransmit of that specific
    /// chunk, or to a partial finalize on hard timeout.
    pub(super) fn try_fec_recover(&mut self) -> Result<()> {
        if self.header_template.parity_count == 0 {
            return Ok(());
        }
        // FEC requires a known chunk_size (set by a non-final DATA or any PARITY).
        let Some(chunk_size) = self.chunk_size else {
            return Ok(());
        };
        let total_data = self.header_template.total_data as usize;
        let parity_count = self.header_template.parity_count as usize;
        let present = self.received_data as usize + self.received_parity as usize;
        if present < total_data {
            return Ok(());
        }
        // Guard against the silent-truncation case: the final DATA chunk is
        // missing and we never observed its real length. RS would happily
        // hand us back `chunk_size` zero-padded bytes that we cannot trim.
        // Wait for the real frame (via NACK) or timeout instead.
        let last_idx = total_data - 1;
        if total_data > 0 && self.data_shards[last_idx].is_none() && self.last_data_len.is_none() {
            return Ok(());
        }

        // Build the combined shard vector for the RS coder. Pad the last
        // data shard up to `chunk_size` so all shards are equal-sized.
        let mut shards: Vec<Option<Vec<u8>>> = Vec::with_capacity(total_data + parity_count);
        for (idx, slot) in self.data_shards.iter().enumerate() {
            shards.push(slot.as_ref().map(|p| {
                if idx == total_data - 1 && p.len() < chunk_size {
                    let mut padded = vec![0u8; chunk_size];
                    padded[..p.len()].copy_from_slice(p);
                    padded
                } else {
                    p.clone()
                }
            }));
        }
        for slot in &self.parity_shards {
            shards.push(slot.clone());
        }

        let rs = ReedSolomon::new(total_data, parity_count)
            .map_err(|e| VoiceError::Fec(e.to_string()))?;
        rs.reconstruct_data(&mut shards)
            .map_err(|e| VoiceError::Fec(e.to_string()))?;

        // Pull recovered data shards back into self.data_shards.
        for (idx, slot) in shards.into_iter().take(total_data).enumerate() {
            if self.data_shards[idx].is_none()
                && let Some(payload) = slot
            {
                let trimmed = if idx == total_data - 1
                    && let Some(real_len) = self.last_data_len
                {
                    payload[..real_len].to_vec()
                } else {
                    payload
                };
                self.data_shards[idx] = Some(trimmed);
                self.received_data = self.received_data.saturating_add(1);
                self.recovered_via_fec = self.recovered_via_fec.saturating_add(1);
            }
        }
        Ok(())
    }

    pub(super) fn missing_data_indices(&self) -> Vec<u8> {
        self.data_shards
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.is_none().then_some(i as u8))
            .collect()
    }
}

/// Shared in-progress reassembly table + per-sender counts + recent
/// completion/eviction blacklist.
pub(super) struct AssemblerInner {
    pub(super) in_progress: HashMap<SenderKey, AssemblyState>,
    /// Per-sender count of in-progress entries, for rate-limiting.
    pub(super) per_sender: HashMap<Arc<str>, usize>,
    pub(super) blacklist: Vec<(SenderKey, Instant)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::types::VoiceDestination;

    fn sample_header() -> ChunkHeader {
        ChunkHeader {
            packet_type: crate::voice::types::PacketType::Data,
            encrypted: false,
            last_in_stream: false,
            message_id: 12345,
            codec: crate::voice::types::VoiceCodec::AmrNb,
            codec_param: 5,
            stream_seq: 0,
            chunk_index: 0,
            total_data: 3,
            parity_count: 0,
            mac_keyed: false,
        }
    }

    #[test]
    fn assembly_state_new() {
        let header = sample_header();
        let state = AssemblyState::new(header, Some(64), VoiceDestination::Broadcast, 0);

        assert_eq!(state.header_template, header);
        assert_eq!(state.chunk_size, Some(64));
        assert_eq!(state.data_shards.len(), 3);
        assert_eq!(state.parity_shards.len(), 0);
        assert_eq!(state.received_data, 0);
        assert_eq!(state.received_parity, 0);
        assert_eq!(state.nack_rounds, 0);
        assert_eq!(state.validation_strikes, 0);
    }

    #[test]
    fn assembly_state_with_parity() {
        let mut header = sample_header();
        header.parity_count = 2;
        let state = AssemblyState::new(header, Some(64), VoiceDestination::Broadcast, 0);

        assert_eq!(state.parity_shards.len(), 2);
    }

    #[test]
    fn missing_data_indices_all() {
        let header = sample_header();
        let state = AssemblyState::new(header, Some(64), VoiceDestination::Broadcast, 0);

        let missing = state.missing_data_indices();
        assert_eq!(missing, vec![0, 1, 2]);
    }

    #[test]
    fn missing_data_indices_partial() {
        let header = sample_header();
        let mut state = AssemblyState::new(header, Some(64), VoiceDestination::Broadcast, 0);

        state.data_shards[0] = Some(vec![1, 2, 3]);
        state.data_shards[2] = Some(vec![4, 5, 6]);

        let missing = state.missing_data_indices();
        assert_eq!(missing, vec![1]);
    }

    #[test]
    fn missing_data_indices_none() {
        let header = sample_header();
        let mut state = AssemblyState::new(header, Some(64), VoiceDestination::Broadcast, 0);

        state.data_shards[0] = Some(vec![1, 2, 3]);
        state.data_shards[1] = Some(vec![4, 5, 6]);
        state.data_shards[2] = Some(vec![7, 8, 9]);

        let missing = state.missing_data_indices();
        assert_eq!(missing, vec![]);
    }

    #[test]
    fn try_fec_recover_no_parity() {
        let header = sample_header();
        let mut state = AssemblyState::new(header, Some(64), VoiceDestination::Broadcast, 0);

        let result = state.try_fec_recover();
        assert!(result.is_ok());
        assert_eq!(state.recovered_via_fec, 0);
    }

    #[test]
    fn try_fec_recover_no_chunk_size() {
        let mut header = sample_header();
        header.parity_count = 1;
        let mut state = AssemblyState::new(header, None, VoiceDestination::Broadcast, 0);

        let result = state.try_fec_recover();
        assert!(result.is_ok());
        assert_eq!(state.recovered_via_fec, 0);
    }

    #[test]
    fn try_fec_recover_insufficient_shards() {
        let mut header = sample_header();
        header.parity_count = 2;
        let mut state = AssemblyState::new(header, Some(64), VoiceDestination::Broadcast, 0);

        state.data_shards[0] = Some(vec![1u8; 64]);
        state.received_data = 1;

        let result = state.try_fec_recover();
        assert!(result.is_ok());
        assert_eq!(state.recovered_via_fec, 0);
    }

    #[test]
    fn try_fec_recover_final_chunk_unknown_length() {
        let mut header = sample_header();
        header.parity_count = 2;
        let mut state = AssemblyState::new(header, Some(64), VoiceDestination::Broadcast, 0);

        state.data_shards[0] = Some(vec![1u8; 64]);
        state.data_shards[1] = Some(vec![2u8; 64]);
        state.data_shards[2] = None;
        state.parity_shards[0] = Some(vec![3u8; 64]);
        state.parity_shards[1] = Some(vec![4u8; 64]);
        state.received_data = 2;
        state.received_parity = 2;
        state.last_data_len = None;

        let result = state.try_fec_recover();
        assert!(result.is_ok());
        assert_eq!(
            state.recovered_via_fec, 0,
            "should not recover when final chunk length unknown"
        );
    }

    #[test]
    fn try_fec_recover_succeeds() {
        let mut header = sample_header();
        header.total_data = 2;
        header.parity_count = 1;
        let mut state = AssemblyState::new(header, Some(64), VoiceDestination::Broadcast, 0);

        state.data_shards[0] = Some(vec![1u8; 64]);
        state.data_shards[1] = None;
        state.parity_shards[0] = Some(vec![2u8; 64]);
        state.received_data = 1;
        state.received_parity = 1;
        state.last_data_len = Some(32);

        let result = state.try_fec_recover();
        assert!(result.is_ok());
        assert_eq!(state.recovered_via_fec, 1, "should recover 1 chunk");
        assert!(
            state.data_shards[1].is_some(),
            "should recover missing shard"
        );
    }
}
