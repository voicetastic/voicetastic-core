//! Sender-side retransmit registry — canonical implementation.
//!
//! After [`crate::voice::build_message`] produces an [`EncodedMessage`],
//! the sender pushes every frame onto the wire and then the Meshtastic
//! firmware *forgets* the packet. The wire protocol relies on
//! NACK-driven selective retransmission (`VOICE_PROTOCOL.md` §5): when a
//! receiver can't recover a message via FEC alone it emits a bitmap
//! NACK and expects the sender to ship the missing DATA chunks back.
//!
//! [`OutgoingVoiceRegistry`] is that cache. Each time we send a voice
//! message we register its per-frame bytes here keyed by `message_id`.
//! When a NACK arrives back, the caller consumes
//! [`OutgoingVoice::frames_for`] (or, more typically, the higher-level
//! [`OutgoingVoiceRegistry::take_retransmit`]) and resends only the
//! missing chunks.
//!
//! Entries are evicted after the configured retain TTL (see
//! [`OutgoingVoiceRegistry::set_retain_ttl`]) or once
//! [`MAX_RETRANSMITS_PER_MESSAGE`] retransmits have been issued, so the
//! registry never grows unbounded.
//!
//! ## Why the cooldown + pending-chunks set
//!
//! Naïvely honouring every NACK round is unsafe on real LoRa: the peer
//! can fire several NACK rounds while our paced TX is still in flight,
//! causing the same chunks to be re-enqueued multiple times. That
//! saturates both our local voice TX queue and the firmware's outbound
//! queue (`ERRNO_TOO_LARGE` / `res=32`), which presents as a sender
//! that appears stuck for tens of minutes until reboot.
//!
//! `cooldown_until` parks a message after each retransmit batch so the
//! previous frames have time to actually leave the radio before we
//! honour the next NACK. `pending_chunks` deduplicates: a chunk that is
//! already in flight is not re-queued by an overlapping NACK round.
//! Callers must call [`OutgoingVoiceRegistry::mark_chunk_sent`] once
//! the worker has actually transmitted each frame, releasing the slot
//! so a later NACK can request it again if it's *still* missing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::builder::EncodedMessage;

/// Default retain TTL when the app hasn't applied settings yet. Matches
/// the assembler's `message_timeout` default so a NACK never arrives
/// for an entry we've already forgotten.
pub const DEFAULT_RETAIN_TTL: Duration = Duration::from_secs(600);
/// Maximum number of NACK rounds we'll honour per outgoing message.
/// Sized to cover the receiver's worst-case `NACK_MAX_ROUNDS` budget at
/// the top of the configurable reassembly-timeout range
/// (3600 s / 1.5 s ≈ 2400 rounds), so the sender's cap is never the
/// thing that gives up first on a stretched but otherwise healthy
/// delivery. The previous value of `32` (a `u8`) tripped while the
/// receiver was still actively NACKing on slow LoRa presets.
pub const MAX_RETRANSMITS_PER_MESSAGE: u16 = 2_400;

/// A single outgoing voice transmission retained for retransmit.
#[derive(Debug)]
#[allow(dead_code)] // `parity_count`, `channel`, `dest` retained for diagnostics/future use.
pub struct OutgoingVoice {
    /// Wire frames in the order produced by `build_message`. DATA shards
    /// occupy `[0..total_data]`, parity shards occupy `[total_data..]`.
    pub frames: Vec<Vec<u8>>,
    pub total_data: u8,
    pub parity_count: u8,
    pub channel: u32,
    /// Unicast destination, or `None` for broadcast.
    pub dest: Option<u32>,
    pub registered_at: Instant,
    pub retransmits: u16,
    /// Earliest instant at which a new NACK round for this message is
    /// allowed to consume more frames. Set after each `take_retransmit`
    /// to `now + pacing × frames × 2` so the previous batch has time to
    /// actually leave the radio (and reach the peer) before we honour
    /// the next NACK. Without this guard, a remote receiver that fires
    /// several NACK rounds while our paced TX is still in flight
    /// causes the same chunks to be re-enqueued multiple times,
    /// saturating the voice TX queue and the firmware's outbound queue
    /// (`ERRNO_TOO_LARGE`/res=32) — visible as a sender that appears
    /// stuck for tens of minutes until reboot.
    pub cooldown_until: Option<Instant>,
    /// Data-chunk indices currently in flight: either already enqueued
    /// on the voice TX worker or just handed to the radio but not yet
    /// confirmed. `take_retransmit` filters incoming NACK lists against
    /// this set and adds the indices it returns; the watcher calls
    /// [`OutgoingVoiceRegistry::mark_chunk_sent`] when the worker has
    /// actually transmitted the frame, releasing the slot so a later
    /// NACK can request it again.
    pub pending_chunks: HashSet<u8>,
}

impl OutgoingVoice {
    /// Collect `(chunk_index, wire_frame)` pairs for chunks listed in
    /// `missing` that are *not* already in flight. Indices outside
    /// `[0..total_data]` and chunks already present in `pending_chunks`
    /// are skipped.
    pub fn frames_for(&self, missing: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let total = self.total_data as usize;
        missing
            .iter()
            .filter_map(|&idx| {
                let i = idx as usize;
                if i < total && !self.pending_chunks.contains(&idx) {
                    Some((idx, self.frames[i].clone()))
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Thread-safe map keyed by `message_id`.
pub struct OutgoingVoiceRegistry {
    inner: Mutex<HashMap<u32, OutgoingVoice>>,
    /// Retain TTL in seconds. Stored atomically so the (hot) GC path
    /// doesn't need to acquire an extra lock just to read it.
    retain_ttl_secs: AtomicU64,
}

impl Default for OutgoingVoiceRegistry {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            retain_ttl_secs: AtomicU64::new(DEFAULT_RETAIN_TTL.as_secs()),
        }
    }
}

impl OutgoingVoiceRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Override the retain TTL. Typically wired to the same setting that
    /// drives the receiver's `AssemblerConfig::message_timeout`, so the
    /// sender never forgets a frame the receiver may still NACK.
    pub fn set_retain_ttl(&self, ttl: Duration) {
        self.retain_ttl_secs
            .store(ttl.as_secs().max(1), Ordering::Relaxed);
    }

    fn retain_ttl(&self) -> Duration {
        Duration::from_secs(self.retain_ttl_secs.load(Ordering::Relaxed))
    }

    /// Number of in-flight outgoing messages currently retained. Diagnostic.
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Returns `true` if no outgoing messages are retained.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    pub fn register(
        &self,
        message_id: u32,
        encoded: &EncodedMessage,
        channel: u32,
        dest: Option<u32>,
    ) {
        let now = Instant::now();
        let ttl = self.retain_ttl();
        let mut map = self.inner.lock();
        // Opportunistic GC.
        map.retain(|_, v| now.duration_since(v.registered_at) < ttl);
        // Seed `pending_chunks` with every DATA index so an early NACK
        // arriving while the *initial burst* is still draining out of
        // the worker queue is naturally dedup'd: the receiver's missing
        // set intersected with `pending_chunks` is empty until the
        // burst loop starts calling `mark_chunk_sent` per frame. This
        // replaces the previous coarse `cooldown × total_data` band-aid
        // and lets us cooldown only against the *actual* retransmit
        // batch we just issued.
        let mut pending_chunks = HashSet::with_capacity(encoded.total_data as usize);
        for i in 0..encoded.total_data {
            pending_chunks.insert(i);
        }
        map.insert(
            message_id,
            OutgoingVoice {
                frames: encoded.frames.clone(),
                total_data: encoded.total_data,
                parity_count: encoded.parity_count,
                channel,
                dest,
                registered_at: now,
                retransmits: 0,
                cooldown_until: None,
                pending_chunks,
            },
        );
    }

    /// Mark a previously-pending chunk as actually transmitted by the
    /// voice TX worker, releasing it so a subsequent NACK can request
    /// it again. No-op if the message has been GC'd or the chunk was
    /// not pending.
    pub fn mark_chunk_sent(&self, message_id: u32, chunk_index: u8) {
        let mut map = self.inner.lock();
        if let Some(entry) = map.get_mut(&message_id) {
            entry.pending_chunks.remove(&chunk_index);
        }
    }

    /// Drop the entry for `message_id`. Idempotent.
    pub fn remove(&self, message_id: u32) {
        self.inner.lock().remove(&message_id);
    }

    /// Number of DATA chunks for a registered message, or `None` if
    /// the entry has been GC'd. Used by [`crate::voice::VoiceSender`]
    /// to bound the per-data-chunk `mark_chunk_sent` calls during the
    /// initial burst (parity frames don't participate in NACKs).
    pub fn data_count(&self, message_id: u32) -> Option<u8> {
        self.inner.lock().get(&message_id).map(|e| e.total_data)
    }

    /// Look up an entry, bump its retransmit counter, and return the frames
    /// to resend. Returns `None` if no entry exists, the TTL elapsed, the
    /// retransmit budget is exhausted, or a previous retransmit for this
    /// message is still draining (cooldown window).
    ///
    /// `pacing` is the current per-frame TX pacing (modem-preset dependent);
    /// it is used to compute the cooldown so the cooldown matches the
    /// time the previous batch needs to actually leave the radio.
    pub fn take_retransmit(
        &self,
        message_id: u32,
        missing: &[u8],
        pacing: Duration,
    ) -> Option<Vec<(u8, Vec<u8>)>> {
        let ttl = self.retain_ttl();
        let mut map = self.inner.lock();
        let entry = map.get_mut(&message_id)?;
        let now = Instant::now();
        if now.duration_since(entry.registered_at) >= ttl {
            map.remove(&message_id);
            return None;
        }
        if entry.retransmits >= MAX_RETRANSMITS_PER_MESSAGE {
            return None;
        }
        if let Some(until) = entry.cooldown_until
            && now < until
        {
            // Previous retransmit batch for this message is still being
            // paced out by the worker (or in flight on the air). Drop
            // this NACK round — once the batch lands the peer will
            // (re)compute its missing-set and NACK again if it really
            // needs more.
            return None;
        }
        // Filter out chunks already in flight for this message so two
        // overlapping NACK rounds can't enqueue the same chunk twice.
        let frames = entry.frames_for(missing);
        if frames.is_empty() {
            return None;
        }
        // Mark these chunks pending; the watcher will clear them via
        // `mark_chunk_sent` once the worker has actually transmitted
        // each frame.
        for (idx, _) in &frames {
            entry.pending_chunks.insert(*idx);
        }
        entry.retransmits = entry.retransmits.saturating_add(1);
        // Cooldown ≈ time the just-issued retransmit batch needs to
        // leave the radio. Scaled by `frames.len()` only — the
        // initial-burst overlap case is now handled by
        // `pending_chunks` being seeded full at `register()` time and
        // drained by the burst loop's `mark_chunk_sent` calls, so an
        // early NACK that overlaps the burst gets dedup'd at the
        // `frames_for` step and never reaches this branch.
        //
        // Lower clamp prevents a degenerate `pacing = 0` from
        // disabling the guard. Upper clamp is a fixed 30 s instead
        // of the retain TTL — a 600 s cooldown would burn the
        // receiver's NACK-round budget (32 rounds at 1.5 s ≈ 48 s)
        // long before the sender ever serviced another retransmit.
        let cooldown = pacing
            .saturating_mul(frames.len() as u32)
            .clamp(Duration::from_secs(1), Duration::from_secs(30))
            .min(ttl);
        entry.cooldown_until = Some(now + cooldown);
        Some(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::builder::{BuildConfig, build_message};
    use crate::voice::types::VoiceCodec;

    fn build_encoded(parity: u8) -> EncodedMessage {
        let audio: Vec<u8> = (0..200).map(|i| (i & 0xFF) as u8).collect();
        let cfg = BuildConfig {
            message_id: 0xABCDEF01,
            stream_seq: 0,
            codec: VoiceCodec::Opus,
            codec_param: 16,
            chunk_size: 32,
            parity_count: parity,
            last_in_stream: false,
            encryption: None,
            mac_key: None,
        };
        build_message(&audio, &cfg).unwrap()
    }

    /// Helper: simulate the burst loop having transmitted every data
    /// chunk so subsequent NACKs are eligible for retransmit. New code
    /// in [`crate::voice::sender::VoiceSender`] does this via
    /// `mark_chunk_sent` per frame; tests collapse it into one call.
    fn drain_initial_burst(reg: &OutgoingVoiceRegistry, message_id: u32, total_data: u8) {
        for i in 0..total_data {
            reg.mark_chunk_sent(message_id, i);
        }
    }

    #[test]
    fn register_then_take_returns_requested_frames() {
        let reg = OutgoingVoiceRegistry::default();
        let encoded = build_encoded(0);
        reg.register(0xABCDEF01, &encoded, 0, Some(42));
        drain_initial_burst(&reg, 0xABCDEF01, encoded.total_data);
        let plan = reg
            .take_retransmit(0xABCDEF01, &[0, 2], Duration::from_millis(10))
            .expect("plan");
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].0, 0);
        assert_eq!(plan[0].1, encoded.frames[0]);
        assert_eq!(plan[1].0, 2);
        assert_eq!(plan[1].1, encoded.frames[2]);
    }

    #[test]
    fn pending_chunks_seeded_at_register_filter_early_nacks() {
        // Brand-new entry: every data chunk is pending until the burst
        // loop calls `mark_chunk_sent`. A NACK arriving in that window
        // returns no plan, regardless of cooldown.
        let reg = OutgoingVoiceRegistry::default();
        let encoded = build_encoded(0);
        reg.register(9, &encoded, 0, None);
        let p = reg.take_retransmit(9, &[0, 1, 2], Duration::from_millis(10));
        assert!(p.is_none(), "all chunks still pending from register");
    }

    #[test]
    fn overlapping_nack_rounds_dedupe_via_pending_chunks() {
        let reg = OutgoingVoiceRegistry::default();
        let encoded = build_encoded(0);
        reg.register(1, &encoded, 0, None);
        drain_initial_burst(&reg, 1, encoded.total_data);
        // First NACK requests {1} — accepted, marked pending, parks
        // message under cooldown.
        let p1 = reg
            .take_retransmit(1, &[1], Duration::from_millis(10))
            .expect("first plan");
        assert_eq!(p1.len(), 1);
        // Second NACK arrives before mark_chunk_sent: blocked by cooldown.
        assert!(
            reg.take_retransmit(1, &[1, 2], Duration::from_millis(10))
                .is_none()
        );
    }

    #[test]
    fn mark_chunk_sent_releases_pending_slot() {
        let reg = OutgoingVoiceRegistry::default();
        // Tiny TTL so cooldown clamps to it and lets the next call through.
        reg.set_retain_ttl(Duration::from_secs(3));
        let encoded = build_encoded(0);
        reg.register(1, &encoded, 0, None);
        drain_initial_burst(&reg, 1, encoded.total_data);
        let _ = reg
            .take_retransmit(1, &[1], Duration::from_millis(0))
            .expect("first plan");
        reg.mark_chunk_sent(1, 1);
        // Chunk 1 is no longer pending. A future NACK can request it
        // again (after the cooldown elapses).
        let map = reg.inner.lock();
        let entry = map.get(&1).unwrap();
        assert!(!entry.pending_chunks.contains(&1));
    }

    #[test]
    fn ttl_expiry_drops_entry_on_take() {
        let reg = OutgoingVoiceRegistry::default();
        // `set_retain_ttl` clamps to whole seconds with a 1 s floor, so
        // we have to actually wait that long for the entry to expire.
        reg.set_retain_ttl(Duration::from_secs(1));
        let encoded = build_encoded(0);
        reg.register(7, &encoded, 0, None);
        drain_initial_burst(&reg, 7, encoded.total_data);
        std::thread::sleep(Duration::from_millis(1100));
        assert!(
            reg.take_retransmit(7, &[0], Duration::from_millis(10))
                .is_none()
        );
        assert!(reg.is_empty());
    }

    #[test]
    fn unknown_message_id_returns_none() {
        let reg = OutgoingVoiceRegistry::default();
        assert!(
            reg.take_retransmit(0xDEAD, &[0], Duration::from_millis(10))
                .is_none()
        );
    }

    #[test]
    fn out_of_range_chunk_index_is_filtered() {
        let reg = OutgoingVoiceRegistry::default();
        let encoded = build_encoded(0);
        let bogus = encoded.total_data; // == one past last valid index
        reg.register(3, &encoded, 0, None);
        drain_initial_burst(&reg, 3, encoded.total_data);
        let p = reg.take_retransmit(3, &[bogus], Duration::from_millis(10));
        assert!(p.is_none(), "all indices filtered ⇒ no plan");
    }

    #[test]
    fn remove_drops_entry() {
        let reg = OutgoingVoiceRegistry::default();
        let encoded = build_encoded(0);
        reg.register(5, &encoded, 0, None);
        assert_eq!(reg.len(), 1);
        reg.remove(5);
        assert!(reg.is_empty());
    }
}
