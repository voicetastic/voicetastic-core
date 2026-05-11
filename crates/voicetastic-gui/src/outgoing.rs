//! Outgoing-voice retransmit registry.
//!
//! Each time we send a voice message we register its per-frame bytes here
//! keyed by `message_id`. When a NACK arrives back, the watcher consumes
//! [`OutgoingVoice::frames_for`] and resends only the missing chunks.
//!
//! Entries are evicted after the configured retain TTL (see
//! [`OutgoingVoiceRegistry::set_retain_ttl`]) or once
//! [`MAX_RETRANSMITS_PER_MESSAGE`] retransmits have been issued, so the
//! registry never grows unbounded.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use voicetastic_core::voice::EncodedMessage;

/// Default retain TTL when the app hasn't applied settings yet. Matches
/// the assembler's `message_timeout` default so a NACK never arrives
/// for an entry we've already forgotten.
pub const DEFAULT_RETAIN_TTL: Duration = Duration::from_secs(600);
/// Maximum number of NACK rounds we'll honour per outgoing message.
/// Bumped from 8 because on lossy LoRa links a single message routinely
/// needs more than one selective retransmit pass to fully close, and
/// the receiver-side `NACK_MAX_ROUNDS` (32) was the real ceiling we
/// were tripping over.
pub const MAX_RETRANSMITS_PER_MESSAGE: u8 = 32;

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
    pub retransmits: u8,
}

impl OutgoingVoice {
    /// Collect the wire frames whose `chunk_index` is listed in `missing`.
    /// Indices outside `[0..total_data]` are ignored.
    pub fn frames_for(&self, missing: &[u8]) -> Vec<Vec<u8>> {
        let total = self.total_data as usize;
        missing
            .iter()
            .filter_map(|&idx| {
                let i = idx as usize;
                if i < total {
                    Some(self.frames[i].clone())
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
            },
        );
    }

    /// Look up an entry, bump its retransmit counter, and return the frames
    /// to resend. Returns `None` if no entry exists, the TTL elapsed, or
    /// the retransmit budget is exhausted.
    pub fn take_retransmit(&self, message_id: u32, missing: &[u8]) -> Option<Vec<Vec<u8>>> {
        let ttl = self.retain_ttl();
        let mut map = self.inner.lock();
        let entry = map.get_mut(&message_id)?;
        if Instant::now().duration_since(entry.registered_at) >= ttl {
            map.remove(&message_id);
            return None;
        }
        if entry.retransmits >= MAX_RETRANSMITS_PER_MESSAGE {
            return None;
        }
        entry.retransmits = entry.retransmits.saturating_add(1);
        Some(entry.frames_for(missing))
    }
}
