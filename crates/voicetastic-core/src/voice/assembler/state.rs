//! Per-message reassembly state and the shared `AssemblerInner` table.

use std::collections::HashMap;
use std::time::Instant;

use reed_solomon_erasure::galois_8::ReedSolomon;

use super::super::error::{Result, VoiceError};
use super::super::header::ChunkHeader;
use super::super::types::VoiceDestination;

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
    pub(super) first_seen: chrono::DateTime<chrono::Utc>,
    pub(super) nack_rounds: u8,
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
    pub(super) in_progress: HashMap<(String, u32), AssemblyState>,
    /// Per-sender count of in-progress entries, for rate-limiting.
    pub(super) per_sender: HashMap<String, usize>,
    pub(super) blacklist: Vec<((String, u32), Instant)>,
}
