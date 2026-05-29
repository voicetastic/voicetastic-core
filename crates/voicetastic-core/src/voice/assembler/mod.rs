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
use web_time::Instant;

use parking_lot::Mutex;
use tracing::{debug, warn};

use super::consts::{
    MAX_BODY_SIZE, MAX_IN_PROGRESS_GLOBAL, MAX_IN_PROGRESS_PER_SENDER, MIN_CHUNK_SIZE,
};
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
    pub round: u16,
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
    /// `tick` and on the next accepted frame. Returns error if the config
    /// violates invariants (e.g. `dead_sender_timeout >= message_timeout`).
    pub fn set_config(&self, cfg: AssemblerConfig) -> std::result::Result<(), String> {
        cfg.validate()?;
        *self.cfg.lock() = cfg;
        Ok(())
    }

    /// Atomically mutate the assembler config in place.
    ///
    /// Prefer this over [`Self::set_config`] when only some fields are
    /// changing. Using `set_config` with `AssemblerConfig { foo: …,
    /// ..Default::default() }` from multiple call sites is racy — each
    /// site clobbers the other's contribution by resetting every field
    /// it doesn't explicitly mention. Returns error if the resulting config
    /// violates invariants.
    pub fn update_config<F: FnOnce(&mut AssemblerConfig)>(
        &self,
        f: F,
    ) -> std::result::Result<(), String> {
        let mut cfg = self.cfg.lock();
        f(&mut cfg);
        cfg.validate()?;
        Ok(())
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
        // Snapshot all config values once before locking `inner` so we never
        // re-acquire `self.cfg` while `inner` is held, and both paths see a
        // consistent view across the frame.
        let cfg_guard = self.cfg.lock();
        let completion_memory = cfg_guard.completion_memory;
        let supported_codecs = cfg_guard.supported_codecs.clone();
        drop(cfg_guard);

        let (header, body) = ChunkHeader::parse(bytes)?;

        // NACK frames bypass assembly.
        if header.packet_type == PacketType::Nack {
            return Ok(AssemblyEvent::Nack(parse_nack_body(&header, body)?));
        }

        let key: SenderKey = (Arc::<str>::from(from), header.message_id);
        let mut inner = self.inner.lock();
        let now = Instant::now();

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
        if let Some(allow) = supported_codecs
            && !allow.contains(&header.codec)
        {
            return Err(VoiceError::UnsupportedCodec(header.codec));
        }

        // V3 carries plaintext bodies (Meshtastic channel encryption
        // provides confidentiality on the wire). We hand the body bytes
        // straight to the per-packet ingest path.
        let plain_body = body.to_vec();

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
        //
        // The counter intentionally accumulates without resetting on a
        // subsequent good frame: a sender that occasionally interleaves
        // a malformed retransmit alongside valid ones should still be
        // evicted within MAX_VALIDATION_STRIKES total mismatches. The
        // saturating `u8` makes overflow a non-issue.
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
        // `last_data_at` proves the sender is still alive; bump it on
        // any frame (including duplicates and parity that fails to
        // recover) so dead-sender detection at line 350 only trips on
        // true silence.
        state.last_data_at = now;
        let received_before = state.received_data;

        let ingest_event = match header.packet_type {
            PacketType::Data => ingest_data(state, &header, plain_body)?,
            PacketType::Parity => ingest_parity(state, &header, plain_body)?,
            PacketType::Nack => unreachable!("handled above"),
        };
        if let Some(ev) = ingest_event {
            return Ok(ev);
        }

        // Try FEC recovery if we now have enough shards.
        if let Err(e) = state.try_fec_recover() {
            warn!(?e, message_id = ?header.message_id, "FEC recovery failed");
        }

        // Reset NACK backoff state only on real DATA progress (direct
        // DATA shard or successful FEC recovery — both bump
        // `received_data`). Parity arrivals that don't recover any
        // missing data shards must NOT reset the round counter or quiet
        // timer: if they did, exponential backoff would never escalate
        // during a stuck transmission where parity keeps trickling in
        // but the data we still need does not. That regression presents
        // as `round=1` NACKs every few seconds for the entire
        // `message_timeout` window, saturating the receiver's own radio
        // queue with NACK retries.
        if state.received_data > received_before {
            state.nack_rounds = 0;
            state.last_chunk_at = now;
        }

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

    /// Drive timeouts and NACK emission. Call periodically (~100 ms cadence).
    pub fn tick(&self) -> TickOutput {
        let cfg = self.cfg.lock().clone();
        let mut inner = self.inner.lock();
        let now = Instant::now();
        prune_blacklist(&mut inner.blacklist, now, cfg.completion_memory);

        let mut finalized_msgs = Vec::new();
        let mut nacks = Vec::new();

        let nack_window = cfg.nack_window;
        let timeout = cfg.message_timeout;
        let max_nack_rounds = cfg.max_nack_rounds;
        let backoff_base = cfg.nack_backoff_base;

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

            // NACK is suppressed for broadcast messages (multiple receivers
            // would all NACK the same chunks, sender has no clear retransmit
            // target) and when the host config explicitly disables it
            // (`nack_backoff_base == 0`, set by `VoiceNackMode::Off`).
            // The state machine still drives timeouts and partial finalize,
            // it just never emits a NACK frame on the wire.
            let nack_enabled =
                backoff_base != 0 && !matches!(state.to, VoiceDestination::Broadcast);

            // Hard timeout — give up. The NACK budget bounds *consecutive*
            // rounds without progress so a sender that's still actively
            // servicing NACKs (one retransmit per round) keeps the slot
            // alive; only true silence trips the cap. `message_timeout`
            // is the absolute upper bound regardless.
            if elapsed_total >= timeout || state.nack_rounds >= max_nack_rounds {
                // Emit one final NACK with `give_up = true` so the sender
                // knows to stop retransmitting. Without this signal the
                // sender continues to burn airtime for its entire linger
                // window. Skipped when NACK is disabled for this entry.
                if nack_enabled {
                    let missing = state.missing_data_indices();
                    let give_up_frame = build_nack(
                        state.header_template.message_id,
                        state.header_template.stream_seq,
                        state.header_template.codec,
                        state.header_template.codec_param,
                        state.header_template.total_data,
                        state.header_template.parity_count,
                        &missing,
                        true,
                    );
                    nacks.push(OutboundNack {
                        from: key.0.to_string(),
                        channel: state.channel,
                        frame: give_up_frame,
                        give_up: true,
                        message_id: state.header_template.message_id,
                        missing_count: missing.len(),
                        round: state.nack_rounds.saturating_add(1),
                    });
                }

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

            // When NACK is disabled there's nothing else to do this tick:
            // we let `message_timeout` fire on its own. (Dead-sender
            // detection is also a NACK-side optimisation — without NACK
            // there's no storm to suppress.)
            if !nack_enabled {
                continue;
            }

            // Dead-sender detection: if no real data (data/parity chunk)
            // arrived for the configured window, the sender is truly gone.
            // Suppress further NACKs and let the hard timeout finalize the
            // message naturally.
            if now.duration_since(state.last_data_at) >= cfg.dead_sender_timeout {
                continue;
            }

            // Quiet-period exceeded ⇒ emit a NACK round. Backoff multiplier
            // is `nack_backoff_base.pow(min(round, 4))`. With the default
            // base of `3`: 1×, 3×, 9×, 27×, 81× — gentle. With `2`
            // (Aggressive / fast-preset Auto): 1×, 2×, 4×, 8×, 16× —
            // more retries per unit time.
            let pow = backoff_base
                .checked_pow(state.nack_rounds.min(4) as u32)
                .unwrap_or(u32::MAX);
            let effective_window = nack_window.saturating_mul(pow);
            if elapsed_quiet >= effective_window
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
                // Advance the quiet timer by the effective window so the
                // next NACK must wait the full backoff-multiplied window.
                state.last_chunk_at = state
                    .last_chunk_at
                    .checked_add(effective_window)
                    .unwrap_or(now);
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
    use crate::node::NodeId;

    fn cfg(parity: u8) -> BuildConfig {
        BuildConfig {
            message_id: 0xCAFEBABE,
            stream_seq: 7,
            codec: VoiceCodec::Opus,
            codec_param: 16,
            chunk_size: 64,
            parity_count: parity,
            last_in_stream: false,
        }
    }

    /// Default test destination — unicast so the NACK emission branches
    /// fire. Tests that explicitly exercise broadcast suppression use
    /// `VoiceDestination::Broadcast` directly.
    fn unicast_dest() -> VoiceDestination {
        VoiceDestination::Node(NodeId::from_u32(0xDEAD_BEEF))
    }

    #[test]
    fn tick_emits_nack_after_quiet_window() {
        let audio: Vec<u8> = (0..(64 * 3)).map(|i| (i & 0xff) as u8).collect();
        let enc = build_message(&audio, &cfg(0)).unwrap();
        let asm = VoiceAssembler::new(AssemblerConfig::default());
        let _ = asm.accept("!cc", unicast_dest(), 0, &enc.frames[0]);
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

    /// Regression for the "phantom partial" bug: a sender that is still
    /// actively servicing every NACK round (one retransmit per round)
    /// must NOT be killed off by the round cap. Only `max_nack_rounds`
    /// of *consecutive silence with no progress at all* should give up;
    /// `message_timeout` remains the absolute upper bound.
    ///
    /// The earlier "cumulative" semantic (a separate counter that never
    /// reset on progress) tripped on healthy slow-trickle messages —
    /// 32 rounds of "burst → quiet → NACK → retransmit → progress" was
    /// indistinguishable from 32 rounds of pure silence and surfaced as
    /// the dreaded `partial: 47/51 chunks` line before the sender had
    /// even finished transmitting.
    #[test]
    fn servicing_sender_is_not_killed_by_round_cap() {
        // 6 data chunks, no FEC. Round cap = 3 — easy to exceed.
        let audio: Vec<u8> = (0..(64 * 6)).map(|i| (i & 0xff) as u8).collect();
        let enc = build_message(&audio, &cfg(0)).unwrap();
        let asm = VoiceAssembler::new(AssemblerConfig {
            max_nack_rounds: 3,
            partial_play_on_timeout: true,
            ..Default::default()
        });

        // Deliver chunk 0 to establish the assembly slot.
        let _ = asm.accept("!cc", unicast_dest(), 0, &enc.frames[0]);

        // 5 NACK rounds, each followed by a real shard from the sender.
        // Total > max_nack_rounds, but every round produces progress so
        // the consecutive counter resets and we keep going.
        for round in 1..=5u8 {
            {
                let mut inner = asm.inner.lock();
                for (_, st) in inner.in_progress.iter_mut() {
                    st.last_chunk_at = Instant::now() - Duration::from_millis(NACK_WINDOW_MS + 100);
                }
            }
            let out = asm.tick();
            assert_eq!(out.nacks.len(), 1, "round {round}: expected 1 NACK");
            assert!(
                out.finalized.is_empty(),
                "round {round}: must not partial-finalize while sender is still servicing rounds",
            );
            // Sender services the NACK with one new shard. Cycle through
            // the remaining indices so we can run more rounds than there
            // are missing chunks.
            let idx = ((round as usize) % (enc.frames.len() - 1)) + 1;
            let _ = asm.accept("!cc", unicast_dest(), 0, &enc.frames[idx]);
        }
    }

    /// Regression: parity frames that arrive but DON'T recover any
    /// missing data shards must NOT reset the NACK round counter.
    ///
    /// Field-observed bug: during a stuck transmission, the sender's
    /// firmware queue saturates and stops servicing DATA retransmits,
    /// but parity continues trickling in. Each parity ingest was
    /// (incorrectly) hitting the "progress" branch and resetting
    /// `nack_rounds = 0`, so exponential backoff never escalated past
    /// round 1 and the receiver storm-NACKed every few seconds for the
    /// entire `message_timeout` window — saturating its OWN radio queue
    /// in turn (`res=32` on outbound NACK).
    #[test]
    fn parity_without_recovery_does_not_reset_round_counter() {
        // 4 data + 2 parity. With only 1 data shard delivered + parity
        // shards, FEC can't recover (needs ≥ total_data shards).
        let audio: Vec<u8> = (0..(64 * 4)).map(|i| (i & 0xff) as u8).collect();
        let enc = build_message(&audio, &cfg(2)).unwrap();
        let asm = VoiceAssembler::new(AssemblerConfig::default());

        // Establish the slot with chunk 0 (DATA).
        let _ = asm.accept("!cc", unicast_dest(), 0, &enc.frames[0]);

        // Force a NACK round.
        {
            let mut inner = asm.inner.lock();
            for (_, st) in inner.in_progress.iter_mut() {
                st.last_chunk_at = Instant::now() - Duration::from_millis(NACK_WINDOW_MS + 100);
            }
        }
        let out = asm.tick();
        assert_eq!(out.nacks.len(), 1, "first NACK should fire");

        // Check round counter is now 1.
        let rounds_after_first_nack = {
            let inner = asm.inner.lock();
            inner.in_progress.values().next().unwrap().nack_rounds
        };
        assert_eq!(rounds_after_first_nack, 1);

        // Deliver a PARITY frame (insufficient for FEC). This must NOT
        // reset nack_rounds, because no DATA progress was made.
        let parity_idx = enc.total_data as usize; // first parity frame
        let _ = asm.accept("!cc", unicast_dest(), 0, &enc.frames[parity_idx]);
        let rounds_after_parity = {
            let inner = asm.inner.lock();
            inner.in_progress.values().next().unwrap().nack_rounds
        };
        assert_eq!(
            rounds_after_parity, 1,
            "parity that doesn't recover data must not reset nack_rounds"
        );

        // Deliver a real DATA chunk — THIS must reset the counter.
        let _ = asm.accept("!cc", unicast_dest(), 0, &enc.frames[1]);
        let rounds_after_data = {
            let inner = asm.inner.lock();
            inner.in_progress.values().next().unwrap().nack_rounds
        };
        assert_eq!(
            rounds_after_data, 0,
            "real data progress must reset nack_rounds"
        );
    }

    /// Counter-regression: with NO sender response at all, the
    /// consecutive cap MUST trip after `max_nack_rounds` quiet windows.
    #[test]
    fn silent_sender_partial_finalizes_after_cap() {
        let audio: Vec<u8> = (0..(64 * 4)).map(|i| (i & 0xff) as u8).collect();
        let enc = build_message(&audio, &cfg(0)).unwrap();
        let asm = VoiceAssembler::new(AssemblerConfig {
            max_nack_rounds: 2,
            partial_play_on_timeout: true,
            ..Default::default()
        });

        // Establish the slot.
        let _ = asm.accept("!cc", unicast_dest(), 0, &enc.frames[0]);

        // Two consecutive NACK rounds with no progress.
        // Each round must set last_chunk_at far enough back to meet the
        // exponential-backoff-multiplied effective window (3^round ×
        // base window), otherwise the backoff check would suppress the
        // NACK until the window is actually exceeded.
        for round in 0..2 {
            let backoff = 3u32.pow((round.min(4)) as u32);
            let ago = Duration::from_millis(NACK_WINDOW_MS * backoff as u64 + 100);
            {
                let mut inner = asm.inner.lock();
                for (_, st) in inner.in_progress.iter_mut() {
                    st.last_chunk_at = Instant::now() - ago;
                }
            }
            let _ = asm.tick();
        }

        // Third tick after another quiet window: cap (=2) reached,
        // partial-finalize and emit ONE final NACK with `give_up = true`
        // so the sender stops retransmitting.
        {
            let mut inner = asm.inner.lock();
            for (_, st) in inner.in_progress.iter_mut() {
                st.last_chunk_at = Instant::now() - Duration::from_millis(NACK_WINDOW_MS * 9 + 100);
            }
        }
        let out = asm.tick();
        assert_eq!(out.nacks.len(), 1, "expected one final give-up NACK");
        assert!(
            out.nacks[0].give_up,
            "the final NACK on cap-reached must carry give_up=true"
        );
        let (h, body) = ChunkHeader::parse(&out.nacks[0].frame).unwrap();
        let info = parse_nack_body(&h, body).unwrap();
        assert!(
            info.give_up,
            "the wire NACK must serialize give_up=true so the sender side parses it"
        );
        assert_eq!(out.finalized.len(), 1, "expected partial finalize");
        assert!(!out.finalized[0].is_complete);
    }

    /// Hard `message_timeout` path also emits a give-up NACK so the
    /// sender can stop retransmitting instead of burning airtime for the
    /// entire linger window. Complements the round-cap path covered by
    /// `silent_sender_partial_finalizes_after_cap`.
    #[test]
    fn message_timeout_emits_give_up_nack() {
        let audio: Vec<u8> = (0..(64 * 3)).map(|i| (i & 0xff) as u8).collect();
        let enc = build_message(&audio, &cfg(0)).unwrap();
        let asm = VoiceAssembler::new(AssemblerConfig {
            message_timeout: Duration::from_millis(50),
            partial_play_on_timeout: true,
            ..Default::default()
        });

        // Establish the slot with chunk 0.
        let _ = asm.accept("!cc", unicast_dest(), 0, &enc.frames[0]);

        // Force the message past its hard timeout.
        {
            let mut inner = asm.inner.lock();
            for (_, st) in inner.in_progress.iter_mut() {
                st.started_at = Instant::now() - Duration::from_millis(60);
            }
        }
        let out = asm.tick();
        assert_eq!(out.nacks.len(), 1, "timeout path must emit give-up NACK");
        assert!(out.nacks[0].give_up);
        assert_eq!(out.finalized.len(), 1);
        assert!(!out.finalized[0].is_complete);
    }

    /// Broadcast messages must NOT emit NACKs — multiple receivers would
    /// otherwise pile retransmit requests onto the sender, with no way
    /// for the sender to know which receiver to retransmit to. The state
    /// machine still drives timeouts and partial-finalize; it just keeps
    /// the wire silent.
    #[test]
    fn broadcast_suppresses_nack_on_quiet_window() {
        let audio: Vec<u8> = (0..(64 * 3)).map(|i| (i & 0xff) as u8).collect();
        let enc = build_message(&audio, &cfg(0)).unwrap();
        let asm = VoiceAssembler::new(AssemblerConfig::default());
        let _ = asm.accept("!cc", VoiceDestination::Broadcast, 0, &enc.frames[0]);
        {
            let mut inner = asm.inner.lock();
            for (_, st) in inner.in_progress.iter_mut() {
                st.last_chunk_at = Instant::now() - Duration::from_millis(NACK_WINDOW_MS * 10);
            }
        }
        let out = asm.tick();
        assert!(
            out.nacks.is_empty(),
            "broadcast must never emit a NACK on quiet window"
        );
    }

    /// Broadcast also gets no give-up NACK on hard timeout — partial
    /// finalize fires as usual but no wire frame is emitted.
    #[test]
    fn broadcast_suppresses_give_up_nack_on_timeout() {
        let audio: Vec<u8> = (0..(64 * 3)).map(|i| (i & 0xff) as u8).collect();
        let enc = build_message(&audio, &cfg(0)).unwrap();
        let asm = VoiceAssembler::new(AssemblerConfig {
            message_timeout: Duration::from_millis(50),
            partial_play_on_timeout: true,
            ..Default::default()
        });
        let _ = asm.accept("!cc", VoiceDestination::Broadcast, 0, &enc.frames[0]);
        {
            let mut inner = asm.inner.lock();
            for (_, st) in inner.in_progress.iter_mut() {
                st.started_at = Instant::now() - Duration::from_millis(60);
            }
        }
        let out = asm.tick();
        assert!(
            out.nacks.is_empty(),
            "broadcast timeout must finalize partial but emit no NACK"
        );
        assert_eq!(out.finalized.len(), 1, "partial finalize still fires");
        assert!(!out.finalized[0].is_complete);
    }

    /// When `nack_backoff_base == 0` (set by `VoiceNackMode::Off`), the
    /// assembler must skip NACK emission even on unicast.
    #[test]
    fn backoff_base_zero_disables_nack() {
        let audio: Vec<u8> = (0..(64 * 3)).map(|i| (i & 0xff) as u8).collect();
        let enc = build_message(&audio, &cfg(0)).unwrap();
        let asm = VoiceAssembler::new(AssemblerConfig {
            nack_backoff_base: 0,
            ..Default::default()
        });
        let _ = asm.accept("!cc", unicast_dest(), 0, &enc.frames[0]);
        {
            let mut inner = asm.inner.lock();
            for (_, st) in inner.in_progress.iter_mut() {
                st.last_chunk_at = Instant::now() - Duration::from_millis(NACK_WINDOW_MS * 10);
            }
        }
        let out = asm.tick();
        assert!(
            out.nacks.is_empty(),
            "Off mode (backoff_base = 0) must suppress all NACK emission"
        );
    }

    /// `backoff_base = 2` doubles the effective quiet window each round
    /// instead of tripling. Verifies the configurable base wires through.
    #[test]
    fn backoff_base_two_doubles_per_round() {
        let audio: Vec<u8> = (0..(64 * 6)).map(|i| (i & 0xff) as u8).collect();
        let enc = build_message(&audio, &cfg(0)).unwrap();
        let asm = VoiceAssembler::new(AssemblerConfig {
            nack_window: Duration::from_millis(1_000),
            nack_backoff_base: 2,
            max_nack_rounds: 10,
            ..Default::default()
        });

        let _ = asm.accept("!cc", unicast_dest(), 0, &enc.frames[0]);

        // Round 0: 1× window = 1 s. Round 1: 2× = 2 s. Round 2: 4× = 4 s.
        // Push last_chunk_at far enough back to clear each round in turn.
        for round in 0..3u32 {
            let multiplier = 2u32.pow(round);
            let ago = Duration::from_millis(1_000 * multiplier as u64 + 100);
            {
                let mut inner = asm.inner.lock();
                for (_, st) in inner.in_progress.iter_mut() {
                    st.last_chunk_at = Instant::now() - ago;
                }
            }
            let out = asm.tick();
            assert_eq!(
                out.nacks.len(),
                1,
                "round {round}: expected 1 NACK with backoff_base=2"
            );
        }
    }
}
