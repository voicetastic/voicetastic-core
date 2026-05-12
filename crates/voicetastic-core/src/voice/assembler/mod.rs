//! Receive-side state machine: reassembly, FEC recovery, NACK emission.
//!
//! This module is split across:
//! - [`config`]   — public [`AssemblerConfig`] + defaults.
//! - [`state`]    — `AssemblyState` and `AssemblerInner` (private).
//! - [`finalize`] — `finalize()` + `push_blacklist()` (private).
//! - this file    — public [`VoiceAssembler`] surface and the
//!   ingest/tick pipeline.

mod config;
mod finalize;
mod state;

pub use config::AssemblerConfig;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use tracing::{debug, warn};

use super::consts::{
    HEADER_SIZE, MAX_BODY_SIZE, MAX_IN_PROGRESS_GLOBAL, MAX_IN_PROGRESS_PER_SENDER, MIN_CHUNK_SIZE,
};
use super::crypto::{decrypt_body, derive_key};
use super::error::{Result, VoiceError};
use super::header::ChunkHeader;
use super::message::{AssemblyEvent, VoiceMessage};
use super::nack::{build_nack, parse_nack_body};
use super::types::{PacketType, VoiceDestination};

use config::MAX_VALIDATION_STRIKES;
use finalize::{finalize, push_blacklist};
use state::{AssemblerInner, AssemblyState, SenderKey};

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
    /// `message_id` of the in-progress message this NACK is for.
    /// Diagnostic only.
    pub message_id: u32,
    /// Number of missing data indices this round is requesting.
    /// Diagnostic only.
    pub missing_count: usize,
    /// 1-based round number (1 = first NACK, 2 = second, …).
    /// Diagnostic only.
    pub round: u8,
}

/// Receive-side state machine.
///
/// Hand it raw `PRIVATE_APP` payload bytes via [`Self::accept`]; periodically
/// call [`Self::tick`] to drive timeouts and emit NACKs.
pub struct VoiceAssembler {
    inner: Mutex<AssemblerInner>,
    cfg: Mutex<AssemblerConfig>,
}

impl VoiceAssembler {
    pub fn new(cfg: AssemblerConfig) -> Self {
        Self {
            inner: Mutex::new(AssemblerInner {
                in_progress: HashMap::new(),
                per_sender: HashMap::new(),
                blacklist: Vec::new(),
            }),
            cfg: Mutex::new(cfg),
        }
    }

    /// Hot-replace the assembler config. New values take effect on the next
    /// `tick` and on the next accepted frame.
    pub fn set_config(&self, cfg: AssemblerConfig) {
        *self.cfg.lock() = cfg;
    }

    /// Atomically mutate the assembler config in place.
    ///
    /// Prefer this over [`Self::set_config`] when only some fields are
    /// changing. Using `set_config` with `AssemblerConfig { foo: …,
    /// ..Default::default() }` from multiple call sites is racy — each
    /// site clobbers the other's contribution by resetting every field
    /// it doesn't explicitly mention.
    pub fn update_config<F: FnOnce(&mut AssemblerConfig)>(&self, f: F) {
        let mut cfg = self.cfg.lock();
        f(&mut cfg);
    }

    /// Strict parse of a Meshtastic `"!aabbccdd"` id.
    /// Returns `None` if the format is malformed (encryption requires this).
    fn parse_from_node_num(from: &str) -> Option<u32> {
        let hex = from.strip_prefix('!')?;
        if hex.len() != 8 {
            return None;
        }
        u32::from_str_radix(hex, 16).ok()
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

        let key: SenderKey = (Arc::<str>::from(from), header.message_id);
        let mut inner = self.inner.lock();
        let now = Instant::now();

        let completion_memory = self.cfg.lock().completion_memory;
        prune_blacklist(&mut inner.blacklist, now, completion_memory);
        if inner.blacklist.iter().any(|(k, _)| *k == key) {
            return Err(VoiceError::Blacklisted);
        }

        // Apply per-sender / global caps only when this is a *new* message.
        if !inner.in_progress.contains_key(&key) {
            apply_caps(&mut inner, &key.0, now)?;
        }

        // Reject unknown codecs (spec §3.2).
        if let super::types::VoiceCodec::Unknown(b) = header.codec {
            return Err(VoiceError::UnknownCodec(b));
        }
        // Reject codecs the receiver explicitly doesn't support, so we don't
        // waste a per-sender slot reassembling a message we can't play back.
        if let Some(allow) = &self.cfg.lock().supported_codecs
            && !allow.contains(&header.codec)
        {
            return Err(VoiceError::UnsupportedCodec(header.codec));
        }

        // Decrypt body if needed (uses original header bytes as AAD).
        let plain_body = self.decrypt_if_needed(from, &header, bytes, body)?;

        // Establish or look up the per-message slot.
        let initial_chunk_size = derive_initial_chunk_size(&header, &plain_body)?;
        let state = match inner.in_progress.get_mut(&key) {
            Some(s) => s,
            None => {
                let cnt = inner.per_sender.entry(Arc::clone(&key.0)).or_default();
                *cnt = cnt.saturating_add(1);
                inner
                    .in_progress
                    .entry(key.clone())
                    .or_insert_with(|| AssemblyState::new(header, initial_chunk_size, to, channel))
            }
        };

        // Validate consistency vs. the established header template. After
        // MAX_VALIDATION_STRIKES the entry is evicted + blacklisted.
        if let Err(err) = validate_template(state, &header) {
            state.validation_strikes = state.validation_strikes.saturating_add(1);
            if state.validation_strikes >= MAX_VALIDATION_STRIKES {
                inner.in_progress.remove(&key);
                let cnt = inner.per_sender.entry(Arc::clone(&key.0)).or_default();
                *cnt = cnt.saturating_sub(1);
                push_blacklist(&mut inner.blacklist, key, now);
            }
            return Err(err);
        }
        // Spec §5: parity_count MAY grow on retransmit but MUST NOT shrink.
        // Validation passed (parity_count >= first observed); if it grew we
        // must resize `parity_shards` before any ingest_parity call below,
        // otherwise an index in `[first_parity_count..new_parity_count)`
        // would panic on out-of-bounds slot access.
        if header.parity_count > state.header_template.parity_count {
            let new_len = header.parity_count as usize;
            state.parity_shards.resize(new_len, None);
            state.header_template.parity_count = header.parity_count;
        }
        state.encrypted_seen = state.encrypted_seen || header.encrypted;
        state.last_chunk_at = now;

        let ingest_event = match header.packet_type {
            PacketType::Data => ingest_data(state, &header, plain_body)?,
            PacketType::Parity => ingest_parity(state, &header, plain_body)?,
            PacketType::Nack => unreachable!("handled above"),
        };
        if let Some(ev) = ingest_event {
            return Ok(ev);
        }

        // Progress! A new chunk landed, so reset the *consecutive* NACK
        // round counter. This drives the wire-visible `round` field on
        // future NACKs and means a long, slow-trickling message keeps
        // reporting rounds 1..N rather than monotonically counting up.
        //
        // The hard give-up bound is `total_nack_rounds` (never reset), so
        // a malicious or merely chatty sender that trickles one shard per
        // NACK window can no longer keep an assembly slot alive past
        // `max_nack_rounds` cumulative rounds.
        state.nack_rounds = 0;

        // Try FEC recovery if we now have enough shards.
        let _ = state.try_fec_recover();

        if state.received_data == state.header_template.total_data {
            // Invariant: we just held a `&mut state` borrowed from `in_progress`
            // via `get_mut(&key)` above (now dropped), so the entry must still
            // exist. Guard defensively against future refactors instead of
            // panicking on an unreachable race.
            let Some(state) = inner.in_progress.remove(&key) else {
                return Ok(AssemblyEvent::Pending {
                    message_id: header.message_id,
                    from: from.to_string(),
                    received_data: header.total_data,
                    total_data: header.total_data,
                    channel,
                });
            };
            let cnt = inner.per_sender.entry(Arc::clone(&key.0)).or_default();
            *cnt = cnt.saturating_sub(1);
            push_blacklist(&mut inner.blacklist, key.clone(), now);
            let msg = finalize(from, &key, state, true);
            return Ok(AssemblyEvent::Complete(Box::new(msg)));
        }

        Ok(AssemblyEvent::Pending {
            message_id: state.header_template.message_id,
            from: from.to_string(),
            received_data: state.received_data,
            total_data: state.header_template.total_data,
            channel,
        })
    }

    fn decrypt_if_needed(
        &self,
        from: &str,
        header: &ChunkHeader,
        bytes: &[u8],
        body: &[u8],
    ) -> Result<Vec<u8>> {
        if !header.encrypted {
            return Ok(body.to_vec());
        }
        let psk = {
            let cfg = self.cfg.lock();
            cfg.channel_psk
                .as_ref()
                .ok_or(VoiceError::EncryptedNoPsk)?
                .clone()
        };
        // Spec §7: encrypted frames MUST carry a valid !hex8 `from`.
        let from_node = Self::parse_from_node_num(from)
            .ok_or_else(|| VoiceError::BadFromForEncrypted(from.to_string()))?;
        let derived = derive_key(&psk, header.message_id, from_node);
        decrypt_body(&derived, &bytes[..HEADER_SIZE], body)
    }

    /// Drive timeouts and NACK emission. Call periodically (~100 ms cadence).
    pub fn tick(&self) -> TickOutput {
        let mut inner = self.inner.lock();
        let now = Instant::now();
        let cfg = self.cfg.lock().clone();
        prune_blacklist(&mut inner.blacklist, now, cfg.completion_memory);

        let mut finalized_msgs = Vec::new();
        let mut nacks = Vec::new();

        let nack_window = cfg.nack_window;
        let timeout = cfg.message_timeout;
        let max_nack_rounds = cfg.max_nack_rounds;

        // Snapshot keys to satisfy the borrow checker. Cloning each
        // `SenderKey` here is just an `Arc<str>` refcount bump + a `u32`
        // copy — cheap even with the per-sender / global cap maxed out.
        let keys: Vec<SenderKey> = inner.in_progress.keys().cloned().collect();
        for key in keys {
            let Some(state) = inner.in_progress.get_mut(&key) else {
                continue;
            };
            let elapsed_total = now.duration_since(state.started_at);
            let elapsed_quiet = now.duration_since(state.last_chunk_at);

            // Hard timeout — give up. The total NACK budget is cumulative
            // (never reset on progress) so a trickle-feeder cannot keep the
            // slot alive indefinitely; `nack_rounds` is only used for the
            // wire `round` field.
            if elapsed_total >= timeout || state.total_nack_rounds >= max_nack_rounds {
                // The key came from the snapshot above; the only way it would
                // be missing here is a concurrent mutation, which can't happen
                // under `&mut inner`. Skip defensively rather than panic.
                let Some(state) = inner.in_progress.remove(&key) else {
                    continue;
                };
                let cnt = inner.per_sender.entry(Arc::clone(&key.0)).or_default();
                *cnt = cnt.saturating_sub(1);
                push_blacklist(&mut inner.blacklist, key.clone(), now);
                if cfg.partial_play_on_timeout {
                    finalized_msgs.push(finalize(&key.0, &key, state, false));
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
                    from: key.0.to_string(),
                    channel: state.channel,
                    frame,
                    give_up: false,
                    message_id: state.header_template.message_id,
                    missing_count: missing.len(),
                    round: state.nack_rounds.saturating_add(1),
                });
                state.nack_rounds = state.nack_rounds.saturating_add(1);
                state.total_nack_rounds = state.total_nack_rounds.saturating_add(1);
                state.last_chunk_at = now;
            }
        }

        TickOutput {
            finalized: finalized_msgs,
            nacks,
        }
    }
}

// ---------------------------------------------------------------------------
// Private free functions: pure ingestion / admission helpers. Kept out of
// `impl VoiceAssembler` so the public type stays small and these are easy
// to unit-test independently.
// ---------------------------------------------------------------------------

fn prune_blacklist(bl: &mut Vec<(SenderKey, Instant)>, now: Instant, ttl: std::time::Duration) {
    bl.retain(|(_, t)| now.duration_since(*t) < ttl);
}

/// Enforce per-sender and global in-progress caps. May evict the globally
/// oldest entry to make room.
fn apply_caps(inner: &mut AssemblerInner, from: &Arc<str>, now: Instant) -> Result<()> {
    let in_use = *inner.per_sender.get(from).unwrap_or(&0);
    if in_use >= MAX_IN_PROGRESS_PER_SENDER {
        warn!(from = %from, "voice per-sender cap reached; dropping new message");
        return Err(VoiceError::PerSenderCap(from.to_string()));
    }
    if inner.in_progress.len() >= MAX_IN_PROGRESS_GLOBAL
        && let Some(victim) = inner
            .in_progress
            .iter()
            .min_by_key(|(_, v)| v.started_at)
            .map(|(k, _)| k.clone())
    {
        debug!(victim_from = %victim.0, victim_id = victim.1, "voice global cap; evicting");
        if inner.in_progress.remove(&victim).is_some() {
            let cnt = inner.per_sender.entry(Arc::clone(&victim.0)).or_default();
            *cnt = cnt.saturating_sub(1);
        }
        push_blacklist(&mut inner.blacklist, victim, now);
    }
    Ok(())
}

/// Initial `chunk_size` to seed a new `AssemblyState` with, derived from
/// the first frame's body. Validates the size range.
fn derive_initial_chunk_size(header: &ChunkHeader, plain_body: &[u8]) -> Result<Option<usize>> {
    let cs = match header.packet_type {
        PacketType::Data => {
            let is_last = header.chunk_index == header.total_data - 1;
            if is_last {
                None
            } else {
                Some(plain_body.len())
            }
        }
        PacketType::Parity => Some(plain_body.len()),
        PacketType::Nack => unreachable!("handled before"),
    };
    if let Some(cs) = cs
        && !(MIN_CHUNK_SIZE..=MAX_BODY_SIZE).contains(&cs)
    {
        return Err(VoiceError::ChunkTooLarge {
            got: cs,
            max: MAX_BODY_SIZE,
        });
    }
    Ok(cs)
}

/// Reject frames that disagree with the per-message header template
/// established by the first accepted frame. Caller is responsible for
/// striking + evicting on the returned error.
fn validate_template(state: &AssemblyState, header: &ChunkHeader) -> Result<()> {
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
    if header.stream_seq != state.header_template.stream_seq {
        return Err(VoiceError::StreamSeqMismatch {
            first: state.header_template.stream_seq,
            got: header.stream_seq,
        });
    }
    // Spec §5: parity_count MAY grow on retransmit; receivers MUST accept
    // values >= the first observed and reject decreases. Without this check
    // a later PARITY frame whose index is within the original range but
    // whose `parity_count` shrank would silently reshape RS expectations.
    if header.parity_count < state.header_template.parity_count {
        return Err(VoiceError::ParityCountDecrease {
            first: state.header_template.parity_count,
            got: header.parity_count,
        });
    }
    Ok(())
}

/// Ingest a DATA frame's body into `state`. Returns `Some(event)` for
/// early-return cases (duplicate); `None` means the caller should continue
/// the pipeline (FEC + completion check).
fn ingest_data(
    state: &mut AssemblyState,
    header: &ChunkHeader,
    plain_body: Vec<u8>,
) -> Result<Option<AssemblyEvent>> {
    let idx = header.chunk_index as usize;
    let is_last = idx == state.header_template.total_data as usize - 1;

    // Duplicate check first: a retransmit (correct or tampered) never
    // alters established state, and reporting Duplicate does not leak
    // whether the body matched.
    if state.data_shards[idx].is_some() {
        return Ok(Some(AssemblyEvent::Duplicate));
    }

    if !is_last {
        // Full (non-final) chunks must equal chunk_size, and they fix it
        // if not yet known.
        match state.chunk_size {
            Some(expected) if plain_body.len() != expected => {
                return Err(VoiceError::BodyLenMismatch {
                    got: plain_body.len(),
                    expected,
                });
            }
            None => {
                if !(MIN_CHUNK_SIZE..=MAX_BODY_SIZE).contains(&plain_body.len()) {
                    return Err(VoiceError::ChunkTooLarge {
                        got: plain_body.len(),
                        max: MAX_BODY_SIZE,
                    });
                }
                state.chunk_size = Some(plain_body.len());
            }
            _ => {}
        }
    } else {
        // Final DATA chunk may be shorter than chunk_size, but never longer.
        if let Some(expected) = state.chunk_size
            && plain_body.len() > expected
        {
            return Err(VoiceError::BodyLenMismatch {
                got: plain_body.len(),
                expected,
            });
        }
        state.last_data_len = Some(plain_body.len());
    }

    state.data_shards[idx] = Some(plain_body);
    state.received_data = state.received_data.saturating_add(1);
    Ok(None)
}

/// Ingest a PARITY frame's body into `state`.
fn ingest_parity(
    state: &mut AssemblyState,
    header: &ChunkHeader,
    plain_body: Vec<u8>,
) -> Result<Option<AssemblyEvent>> {
    let idx = header.chunk_index as usize;
    if state.parity_shards[idx].is_some() {
        return Ok(Some(AssemblyEvent::Duplicate));
    }
    match state.chunk_size {
        Some(expected) if plain_body.len() != expected => {
            return Err(VoiceError::BodyLenMismatch {
                got: plain_body.len(),
                expected,
            });
        }
        None => {
            if !(MIN_CHUNK_SIZE..=MAX_BODY_SIZE).contains(&plain_body.len()) {
                return Err(VoiceError::ChunkTooLarge {
                    got: plain_body.len(),
                    max: MAX_BODY_SIZE,
                });
            }
            state.chunk_size = Some(plain_body.len());
        }
        _ => {}
    }
    state.parity_shards[idx] = Some(plain_body);
    state.received_parity = state.received_parity.saturating_add(1);
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::builder::{BuildConfig, build_message};
    use super::super::consts::NACK_WINDOW_MS;
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

    /// Regression for finding #3: a sender that trickles one new shard
    /// just before every NACK window must NOT be able to keep an
    /// assembly slot alive past `max_nack_rounds` cumulative rounds.
    /// Previously `nack_rounds` was reset on every accepted shard, so
    /// the consecutive bound never tripped \u2014 the slot only died at the
    /// hard message timeout.
    #[test]
    fn trickle_feed_cannot_bypass_round_cap() {
        // 6 data chunks, no FEC. Round cap = 3.
        let audio: Vec<u8> = (0..(64 * 6)).map(|i| (i & 0xff) as u8).collect();
        let enc = build_message(&audio, &cfg(0)).unwrap();
        let asm = VoiceAssembler::new(AssemblerConfig {
            max_nack_rounds: 3,
            partial_play_on_timeout: true,
            ..Default::default()
        });

        // Deliver chunk 0 to establish the assembly slot.
        let _ = asm.accept("!cc", VoiceDestination::Broadcast, 0, &enc.frames[0]);

        // Round 1: backdate last_chunk_at, tick, expect a NACK.
        for round in 1..=3 {
            {
                let mut inner = asm.inner.lock();
                for (_, st) in inner.in_progress.iter_mut() {
                    st.last_chunk_at = Instant::now() - Duration::from_millis(NACK_WINDOW_MS + 100);
                }
            }
            let out = asm.tick();
            assert_eq!(out.nacks.len(), 1, "round {round}: expected 1 NACK");
            // Sender "responds" with one new shard \u2014 in the buggy code this
            // resets the round counter and the loop could go forever.
            if round < 3 {
                let _ = asm.accept(
                    "!cc",
                    VoiceDestination::Broadcast,
                    0,
                    &enc.frames[round as usize],
                );
            }
        }

        // Fourth tick after another quiet window: the cumulative cap
        // should now trigger a hard timeout (partial finalize), not
        // another NACK.
        {
            let mut inner = asm.inner.lock();
            for (_, st) in inner.in_progress.iter_mut() {
                st.last_chunk_at = Instant::now() - Duration::from_millis(NACK_WINDOW_MS + 100);
            }
        }
        let out = asm.tick();
        assert!(
            out.nacks.is_empty(),
            "round 4: cumulative cap should suppress NACK, got {:?}",
            out.nacks,
        );
        assert_eq!(out.finalized.len(), 1, "expected partial finalize");
        assert!(!out.finalized[0].is_complete);
    }
}
