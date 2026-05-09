//! Receive-side state machine: reassembly, FEC recovery, NACK emission.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use reed_solomon_erasure::galois_8::ReedSolomon;
use tracing::{debug, warn};

use super::consts::{
    BLACKLIST_MAX, BLACKLIST_TTL, HEADER_SIZE, MAX_BODY_SIZE, MAX_IN_PROGRESS_GLOBAL,
    MAX_IN_PROGRESS_PER_SENDER, MIN_CHUNK_SIZE, NACK_MAX_ROUNDS, NACK_WINDOW_MS,
};
use super::crypto::{decrypt_body, derive_key};
use super::error::{Result, VoiceError};
use super::header::ChunkHeader;
use super::message::{AssemblyEvent, VoiceMessage};
use super::nack::{build_nack, parse_nack_body};
use super::types::{PacketType, VoiceDestination};

/// User-tunable assembler config.
#[derive(Debug, Clone)]
pub struct AssemblerConfig {
    /// Hard timeout per in-progress message.
    pub message_timeout: Duration,
    /// Emit incomplete messages on hard timeout (vs. discard).
    pub partial_play_on_timeout: bool,
    /// If `Some`, the channel PSK used to derive envelope keys for incoming
    /// encrypted frames. `None` ⇒ encrypted frames are dropped.
    pub channel_psk: Option<Vec<u8>>,
}

impl Default for AssemblerConfig {
    fn default() -> Self {
        Self {
            message_timeout: Duration::from_secs(30),
            partial_play_on_timeout: true,
            channel_psk: None,
        }
    }
}

struct AssemblyState {
    header_template: ChunkHeader,
    chunk_size: usize,
    last_data_len: Option<usize>,
    data_shards: Vec<Option<Vec<u8>>>,
    parity_shards: Vec<Option<Vec<u8>>>,
    received_data: u8,
    received_parity: u8,
    started_at: Instant,
    last_chunk_at: Instant,
    first_seen: chrono::DateTime<chrono::Utc>,
    nack_rounds: u8,
    to: VoiceDestination,
    channel: u32,
    encrypted_seen: bool,
    recovered_via_fec: u8,
}

impl AssemblyState {
    fn new(header: ChunkHeader, chunk_size: usize, to: VoiceDestination, channel: u32) -> Self {
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
            to,
            channel,
            encrypted_seen: false,
            recovered_via_fec: 0,
        }
    }

    fn try_fec_recover(&mut self) -> Result<()> {
        if self.header_template.parity_count == 0 {
            return Ok(());
        }
        let total_data = self.header_template.total_data as usize;
        let parity_count = self.header_template.parity_count as usize;
        // Need at least `total_data` shards present in total to attempt recovery.
        let present = self.received_data as usize + self.received_parity as usize;
        if present < total_data {
            return Ok(());
        }
        // Build the combined shard vector for the RS coder.
        let mut shards: Vec<Option<Vec<u8>>> = Vec::with_capacity(total_data + parity_count);
        // Data shards: pad the last one to chunk_size so all shards are equal-sized.
        for (idx, slot) in self.data_shards.iter().enumerate() {
            shards.push(slot.as_ref().map(|p| {
                if idx == total_data - 1 && p.len() < self.chunk_size {
                    let mut padded = vec![0u8; self.chunk_size];
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

    fn missing_data_indices(&self) -> Vec<u8> {
        self.data_shards
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.is_none().then_some(i as u8))
            .collect()
    }
}

/// Receive-side state machine.
///
/// Hand it raw `PRIVATE_APP` payload bytes via [`Self::accept`]; periodically
/// call [`Self::tick`] to drive timeouts and emit NACKs.
pub struct VoiceAssembler {
    inner: Mutex<AssemblerInner>,
    cfg: AssemblerConfig,
}

struct AssemblerInner {
    in_progress: HashMap<(String, u32), AssemblyState>,
    /// Per-sender count of in-progress entries, for rate-limiting.
    per_sender: HashMap<String, usize>,
    blacklist: Vec<((String, u32), Instant)>,
}

/// Outcome of [`VoiceAssembler::tick`].
#[derive(Debug)]
pub struct TickOutput {
    /// Messages finalized this tick (complete or partial).
    pub finalized: Vec<VoiceMessage>,
    /// NACK frames the caller should transmit (already framed; send to the
    /// `from` of the corresponding in-progress message).
    pub nacks: Vec<OutboundNack>,
}

/// A NACK ready for transmission.
#[derive(Debug, Clone)]
pub struct OutboundNack {
    pub from: String,
    pub channel: u32,
    pub frame: Vec<u8>,
    pub give_up: bool,
}

impl VoiceAssembler {
    pub fn new(cfg: AssemblerConfig) -> Self {
        Self {
            inner: Mutex::new(AssemblerInner {
                in_progress: HashMap::new(),
                per_sender: HashMap::new(),
                blacklist: Vec::new(),
            }),
            cfg,
        }
    }

    fn from_node_num(from: &str) -> u32 {
        // "!aabbccdd" → u32. Best-effort; encryption derive_key uses this so a
        // malformed id just yields a different (failing) key.
        from.strip_prefix('!')
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .unwrap_or(0)
    }

    /// Feed a frame.
    pub fn accept(
        &self,
        from: &str,
        to: VoiceDestination,
        channel: u32,
        bytes: &[u8],
    ) -> AssemblyEvent {
        match self.accept_inner(from, to, channel, bytes) {
            Ok(ev) => ev,
            Err(e) => AssemblyEvent::Rejected(e),
        }
    }

    fn accept_inner(
        &self,
        from: &str,
        to: VoiceDestination,
        channel: u32,
        bytes: &[u8],
    ) -> Result<AssemblyEvent> {
        let (header, body) = ChunkHeader::parse(bytes)?;

        // NACK frames bypass assembly.
        if header.packet_type == PacketType::Nack {
            return Ok(AssemblyEvent::Nack(parse_nack_body(&header, body)?));
        }

        let key = (from.to_string(), header.message_id);
        let mut inner = self.inner.lock();

        let now = Instant::now();
        inner
            .blacklist
            .retain(|(_, t)| now.duration_since(*t) < BLACKLIST_TTL);
        if inner.blacklist.iter().any(|(k, _)| *k == key) {
            return Ok(AssemblyEvent::Rejected(VoiceError::TooShort {
                len: 0,
                needed: 0,
            }));
        }

        // Per-sender rate limit, applied only to *new* messages.
        if !inner.in_progress.contains_key(&key) {
            let in_use = *inner.per_sender.get(from).unwrap_or(&0);
            if in_use >= MAX_IN_PROGRESS_PER_SENDER {
                warn!(from, "voice per-sender cap reached; dropping new message");
                return Ok(AssemblyEvent::Rejected(VoiceError::AudioTooLarge {
                    bytes: 0,
                    max: 0,
                }));
            }
            // Global cap — evict the globally-oldest if needed.
            if inner.in_progress.len() >= MAX_IN_PROGRESS_GLOBAL
                && let Some(victim) = inner
                    .in_progress
                    .iter()
                    .min_by_key(|(_, v)| v.started_at)
                    .map(|(k, _)| k.clone())
            {
                debug!(victim_from = %victim.0, victim_id = victim.1, "voice global cap; evicting");
                if let Some(state) = inner.in_progress.remove(&victim) {
                    let cnt = inner.per_sender.entry(victim.0.clone()).or_default();
                    *cnt = cnt.saturating_sub(1);
                    drop(state);
                }
                push_blacklist(&mut inner.blacklist, victim, now);
            }
        }

        // Decrypt body if needed (uses original header bytes as AAD).
        let plain_body: Vec<u8> = if header.encrypted {
            let psk = self
                .cfg
                .channel_psk
                .as_ref()
                .ok_or(VoiceError::BadTag)?
                .as_slice();
            let derived = derive_key(psk, header.message_id, Self::from_node_num(from));
            decrypt_body(&derived, &bytes[..HEADER_SIZE], body)?
        } else {
            body.to_vec()
        };

        // Look up or create the in-progress state.
        let state = match inner.in_progress.get_mut(&key) {
            Some(s) => s,
            None => {
                let chunk_size = match header.packet_type {
                    PacketType::Data => {
                        // First DATA frame fixes chunk_size unless it's the
                        // final (potentially-trimmed) chunk and shorter.
                        if header.chunk_index == header.total_data - 1 {
                            // Defer chunk_size discovery: assume max body for now.
                            plain_body.len().max(MIN_CHUNK_SIZE)
                        } else {
                            plain_body.len()
                        }
                    }
                    PacketType::Parity => plain_body.len(),
                    PacketType::Nack => unreachable!("handled above"),
                };
                if !(MIN_CHUNK_SIZE..=MAX_BODY_SIZE).contains(&chunk_size) {
                    return Err(VoiceError::ChunkTooLarge {
                        got: chunk_size,
                        max: MAX_BODY_SIZE,
                    });
                }
                let cnt = inner.per_sender.entry(from.to_string()).or_default();
                *cnt = cnt.saturating_add(1);
                inner
                    .in_progress
                    .entry(key.clone())
                    .or_insert_with(|| AssemblyState::new(header, chunk_size, to, channel))
            }
        };

        // Validate consistency vs. the established header template.
        if header.total_data != state.header_template.total_data {
            return Err(VoiceError::TotalMismatch {
                first: state.header_template.total_data,
                got: header.total_data,
            });
        }
        if header.codec != state.header_template.codec {
            return Err(VoiceError::CodecMismatch {
                first: state.header_template.codec,
                got: header.codec,
            });
        }
        state.encrypted_seen = state.encrypted_seen || header.encrypted;
        state.last_chunk_at = now;

        match header.packet_type {
            PacketType::Data => {
                let idx = header.chunk_index as usize;
                let is_last = idx == state.header_template.total_data as usize - 1;
                // Body length policy: full chunks must equal chunk_size; the
                // final chunk may be shorter (sender stripped padding).
                if !is_last && plain_body.len() != state.chunk_size {
                    return Err(VoiceError::BodyLenMismatch {
                        got: plain_body.len(),
                        expected: state.chunk_size,
                    });
                }
                if is_last {
                    state.last_data_len = Some(plain_body.len());
                }
                if state.data_shards[idx].is_some() {
                    return Ok(AssemblyEvent::Duplicate);
                }
                state.data_shards[idx] = Some(plain_body);
                state.received_data = state.received_data.saturating_add(1);
            }
            PacketType::Parity => {
                let idx = header.chunk_index as usize;
                if plain_body.len() != state.chunk_size {
                    return Err(VoiceError::BodyLenMismatch {
                        got: plain_body.len(),
                        expected: state.chunk_size,
                    });
                }
                if state.parity_shards[idx].is_some() {
                    return Ok(AssemblyEvent::Duplicate);
                }
                state.parity_shards[idx] = Some(plain_body);
                state.received_parity = state.received_parity.saturating_add(1);
            }
            PacketType::Nack => unreachable!(),
        }

        // Try FEC recovery if we now have enough shards.
        let _ = state.try_fec_recover();

        if state.received_data == state.header_template.total_data {
            let state = inner.in_progress.remove(&key).expect("just inserted");
            let cnt = inner.per_sender.entry(from.to_string()).or_default();
            *cnt = cnt.saturating_sub(1);
            push_blacklist(&mut inner.blacklist, key.clone(), now);
            let msg = finalize(from, &key, state, true);
            return Ok(AssemblyEvent::Complete(Box::new(msg)));
        }

        Ok(AssemblyEvent::Pending)
    }

    /// Drive timeouts and NACK emission. Call periodically (~100 ms cadence).
    pub fn tick(&self) -> TickOutput {
        let mut inner = self.inner.lock();
        let now = Instant::now();
        inner
            .blacklist
            .retain(|(_, t)| now.duration_since(*t) < BLACKLIST_TTL);

        let mut finalized = Vec::new();
        let mut nacks = Vec::new();

        let nack_window = Duration::from_millis(NACK_WINDOW_MS);
        let timeout = self.cfg.message_timeout;

        // Snapshot keys to satisfy the borrow checker.
        let keys: Vec<(String, u32)> = inner.in_progress.keys().cloned().collect();
        for key in keys {
            // We re-fetch each iteration because we may mutate the map.
            let Some(state) = inner.in_progress.get_mut(&key) else {
                continue;
            };
            let elapsed_total = now.duration_since(state.started_at);
            let elapsed_quiet = now.duration_since(state.last_chunk_at);

            // Hard timeout — give up.
            if elapsed_total >= timeout || state.nack_rounds >= NACK_MAX_ROUNDS {
                let state = inner.in_progress.remove(&key).expect("just listed");
                let cnt = inner.per_sender.entry(key.0.clone()).or_default();
                *cnt = cnt.saturating_sub(1);
                push_blacklist(&mut inner.blacklist, key.clone(), now);
                if self.cfg.partial_play_on_timeout {
                    finalized.push(finalize(&key.0, &key, state, false));
                }
                continue;
            }

            // Quiet-period exceeded ⇒ emit a NACK round.
            if elapsed_quiet >= nack_window
                && state.received_data < state.header_template.total_data
            {
                let missing = state.missing_data_indices();
                let frame = build_nack(
                    state.header_template.message_id,
                    state.header_template.stream_seq,
                    state.header_template.codec,
                    state.header_template.codec_param,
                    state.header_template.total_data,
                    state.header_template.parity_count,
                    &missing,
                    false,
                );
                nacks.push(OutboundNack {
                    from: key.0.clone(),
                    channel: state.channel,
                    frame,
                    give_up: false,
                });
                state.nack_rounds = state.nack_rounds.saturating_add(1);
                state.last_chunk_at = now;
            }
        }

        TickOutput { finalized, nacks }
    }
}

fn push_blacklist(bl: &mut Vec<((String, u32), Instant)>, key: (String, u32), now: Instant) {
    if bl.iter().any(|(k, _)| *k == key) {
        return;
    }
    bl.push((key, now));
    if bl.len() > BLACKLIST_MAX {
        let drop = bl.len() - BLACKLIST_MAX;
        bl.drain(0..drop);
    }
}

fn finalize(from: &str, key: &(String, u32), state: AssemblyState, complete: bool) -> VoiceMessage {
    let mut audio = Vec::with_capacity(state.chunk_size * state.data_shards.len());
    for slot in &state.data_shards {
        match slot {
            Some(payload) => audio.extend_from_slice(payload),
            None => {
                // Missing chunk → fill with zeros (codec-specific silence is
                // the responsibility of the decoder/playback layer).
                audio.resize(audio.len() + state.chunk_size, 0);
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

#[cfg(test)]
mod tests {
    use super::super::builder::{BuildConfig, build_message};
    use super::super::nack::parse_nack_body;
    use super::super::types::VoiceCodec;
    use super::*;

    fn cfg(parity: u8) -> BuildConfig {
        BuildConfig {
            message_id: 0xCAFEBABE,
            stream_seq: 7,
            codec: VoiceCodec::Opus,
            codec_param: 16,
            chunk_size: 64,
            parity_count: parity,
            last_in_stream: false,
            encryption: None,
        }
    }

    #[test]
    fn tick_emits_nack_after_quiet_window() {
        let audio: Vec<u8> = (0..(64 * 3)).map(|i| (i & 0xff) as u8).collect();
        let enc = build_message(&audio, &cfg(0)).unwrap();
        let asm = VoiceAssembler::new(AssemblerConfig::default());
        let _ = asm.accept("!cc", VoiceDestination::Broadcast, 0, &enc.frames[0]);
        // Force the in-progress entry's last_chunk_at into the past.
        {
            let mut inner = asm.inner.lock();
            for (_, st) in inner.in_progress.iter_mut() {
                st.last_chunk_at = Instant::now() - Duration::from_millis(NACK_WINDOW_MS + 100);
            }
        }
        let out = asm.tick();
        assert_eq!(out.nacks.len(), 1);
        let (h, body) = ChunkHeader::parse(&out.nacks[0].frame).unwrap();
        let info = parse_nack_body(&h, body).unwrap();
        assert_eq!(info.missing, vec![1, 2]);
    }
}
