//! Voice protocol (v1) — wire-compatible with the Android Voicetastic app.
//!
//! This module is **codec-free**: it only handles the on-the-wire chunk format
//! described in `VOICE_PROTOCOL.md`. AMR-NB capture/encode/decode and audio
//! playback are out of scope; callers feed raw `.amr` file bytes to the chunker
//! and receive raw `.amr` bytes back from the assembler (with the
//! `#!AMR\n` file header re-prepended exactly once).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

/// Current protocol version. Receivers must reject any other value.
pub const PROTOCOL_VERSION: u8 = 1;
/// Fixed header length preceding every chunk.
pub const HEADER_SIZE: usize = 6;
/// Maximum total chunk size (header + payload) — sized to fit a single LoRa packet.
pub const MAX_PACKET_SIZE: usize = 231;
/// Maximum audio bytes per chunk (231 − 6).
pub const MAX_PAYLOAD_SIZE: usize = MAX_PACKET_SIZE - HEADER_SIZE;
/// Recommended inter-chunk delay on send (ms).
pub const INTER_CHUNK_DELAY_MS: u64 = 500;
/// AMR-NB file header (`#!AMR\n`). Stripped on send, re-prepended on receive.
pub const AMR_FILE_HEADER: &[u8] = b"#!AMR\n";
/// AMR-NB NO_DATA frame (frame type 15) — 1 byte, used to fill missing chunks.
pub const AMR_NO_DATA_FRAME: u8 = 0x7C;

/// Maximum chunks allowed in one message (`totalChunks` is `u8`).
pub const MAX_CHUNKS_PER_MESSAGE: usize = 255;

/// Recently-completed message blacklist parameters.
const BLACKLIST_TTL: Duration = Duration::from_secs(60);
const BLACKLIST_MAX: usize = 100;

/// Maximum number of in-progress reassemblies kept at once. Prevents an
/// unbounded `HashMap` if a remote node spams unique `message_id`s without
/// ever finishing a message. When exceeded, the oldest entry (by
/// `started_at`) is evicted on insert.
pub const MAX_IN_PROGRESS: usize = 64;

/// Hard cap on the audio bytes accumulated for a single in-progress
/// message. `MAX_CHUNKS_PER_MESSAGE * MAX_PAYLOAD_SIZE` is the natural ceiling
/// (≈ 57 KB), so this is mostly belt-and-braces against a wire-protocol bug.
pub const MAX_MESSAGE_BYTES: usize = MAX_CHUNKS_PER_MESSAGE * MAX_PAYLOAD_SIZE;

/// Generate a non-zero random `u16` suitable for use as a voice
/// `message_id`. Zero is reserved as "unset" by the chunk header convention.
pub fn random_message_id() -> u16 {
    let mut buf = [0u8; 2];
    getrandom::fill(&mut buf).expect("OS RNG");
    u16::from_ne_bytes(buf).max(1)
}

/// AMR-NB bitrate modes. The ordinal **must** match the Kotlin
/// `AmrNbBitrate` enum order, since we serialise the ordinal in the chunk
/// header (`bitrateIndex`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum AmrNbBitrate {
    /// 4.75 kbps
    Mr475 = 0,
    /// 5.15 kbps
    Mr515 = 1,
    /// 5.90 kbps
    Mr59 = 2,
    /// 6.70 kbps
    Mr67 = 3,
    /// 7.40 kbps
    Mr74 = 4,
    /// 7.95 kbps (default)
    #[default]
    Mr795 = 5,
    /// 10.2 kbps
    Mr102 = 6,
    /// 12.2 kbps
    Mr122 = 7,
}

impl AmrNbBitrate {
    /// Frame size in bytes (including the 1-byte ToC header).
    /// One frame = 20 ms of audio.
    pub fn frame_size(self) -> usize {
        match self {
            Self::Mr475 => 13,
            Self::Mr515 => 14,
            Self::Mr59 => 16,
            Self::Mr67 => 18,
            Self::Mr74 => 20,
            Self::Mr795 => 21,
            Self::Mr102 => 27,
            Self::Mr122 => 32,
        }
    }

    /// Decode from the on-wire ordinal (bitrateIndex byte).
    pub fn from_ordinal(idx: u8) -> Option<Self> {
        Some(match idx {
            0 => Self::Mr475,
            1 => Self::Mr515,
            2 => Self::Mr59,
            3 => Self::Mr67,
            4 => Self::Mr74,
            5 => Self::Mr795,
            6 => Self::Mr102,
            7 => Self::Mr122,
            _ => return None,
        })
    }

    /// On-wire ordinal.
    pub fn ordinal(self) -> u8 {
        self as u8
    }
}

/// User-configurable voice settings (app-only, never sent to the radio).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceConfig {
    pub bitrate: AmrNbBitrate,
    /// Maximum recording length (seconds).
    pub max_duration_seconds: u32,
    /// Reassembly timeout (seconds).
    pub chunk_timeout_seconds: u32,
    /// If true, emit incomplete messages on timeout; if false, discard.
    pub partial_play_on_timeout: bool,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            bitrate: AmrNbBitrate::default(),
            max_duration_seconds: 20,
            chunk_timeout_seconds: 30,
            partial_play_on_timeout: true,
        }
    }
}

/// Reassembled voice message emitted by [`VoiceAssembler`].
#[derive(Debug, Clone)]
pub struct VoiceMessage {
    pub message_id: u16,
    /// Sender node id (`!aabbccdd`).
    pub from: String,
    /// Destination node id, or `"broadcast"`.
    pub to: String,
    /// Reassembled AMR-NB bytes including the `#!AMR\n` file header.
    pub audio_data: Vec<u8>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub is_outgoing: bool,
    pub is_complete: bool,
    pub total_chunks: u8,
    pub received_chunks: u8,
    pub bitrate: AmrNbBitrate,
    pub channel: u32,
}

/// Errors raised by the voice protocol layer.
#[derive(Debug, Error, Clone)]
pub enum VoiceError {
    #[error("packet too short ({len} bytes, need ≥ {needed})")]
    TooShort { len: usize, needed: usize },
    #[error("packet too large ({len} bytes, max {max})")]
    TooLarge { len: usize, max: usize },
    #[error("unsupported protocol version: {0}")]
    BadVersion(u8),
    #[error("invalid bitrate index: {0}")]
    BadBitrate(u8),
    #[error("invalid totalChunks: {0}")]
    BadTotal(u8),
    #[error("chunkIndex {idx} out of range for totalChunks {total}")]
    BadIndex { idx: u8, total: u8 },
    #[error("audio too large: {bytes} B exceeds maximum {max} B per message")]
    AudioTooLarge { bytes: usize, max: usize },
}

/// Parsed view of an inbound chunk header + payload.
#[derive(Debug, Clone)]
pub struct VoiceChunk {
    pub version: u8,
    pub message_id: u16,
    pub chunk_index: u8,
    pub total_chunks: u8,
    pub bitrate: AmrNbBitrate,
    pub payload: Vec<u8>,
}

impl VoiceChunk {
    /// Parse an inbound chunk packet. Validates header version, bounds, and
    /// bitrate index.
    pub fn parse(bytes: &[u8]) -> Result<Self, VoiceError> {
        if bytes.len() < HEADER_SIZE {
            return Err(VoiceError::TooShort {
                len: bytes.len(),
                needed: HEADER_SIZE,
            });
        }
        if bytes.len() > MAX_PACKET_SIZE {
            return Err(VoiceError::TooLarge {
                len: bytes.len(),
                max: MAX_PACKET_SIZE,
            });
        }
        let version = bytes[0];
        if version != PROTOCOL_VERSION {
            return Err(VoiceError::BadVersion(version));
        }
        let message_id = u16::from_be_bytes([bytes[1], bytes[2]]);
        let chunk_index = bytes[3];
        let total_chunks = bytes[4];
        if total_chunks == 0 {
            return Err(VoiceError::BadTotal(total_chunks));
        }
        if chunk_index >= total_chunks {
            return Err(VoiceError::BadIndex {
                idx: chunk_index,
                total: total_chunks,
            });
        }
        let bitrate =
            AmrNbBitrate::from_ordinal(bytes[5]).ok_or(VoiceError::BadBitrate(bytes[5]))?;
        Ok(Self {
            version,
            message_id,
            chunk_index,
            total_chunks,
            bitrate,
            payload: bytes[HEADER_SIZE..].to_vec(),
        })
    }
}

/// Splits an AMR-NB byte stream into wire chunks.
///
/// The 6-byte AMR file header (`#!AMR\n`) is stripped before chunking; only
/// raw frames travel on the wire. The receiver re-adds the header.
pub struct VoiceChunker;

impl VoiceChunker {
    /// Strip `#!AMR\n` if present.
    pub fn strip_amr_header(bytes: &[u8]) -> &[u8] {
        if bytes.starts_with(AMR_FILE_HEADER) {
            &bytes[AMR_FILE_HEADER.len()..]
        } else {
            bytes
        }
    }

    /// Build the 6-byte chunk header.
    pub fn build_header(
        message_id: u16,
        chunk_index: u8,
        total_chunks: u8,
        bitrate: AmrNbBitrate,
    ) -> [u8; HEADER_SIZE] {
        let id = message_id.to_be_bytes();
        [
            PROTOCOL_VERSION,
            id[0],
            id[1],
            chunk_index,
            total_chunks,
            bitrate.ordinal(),
        ]
    }

    /// Chunk an AMR-NB byte stream (with or without the file header) into
    /// wire packets ≤ 231 bytes each. Returns `Err(AudioTooLarge)` if the
    /// payload would require more than 255 chunks.
    pub fn chunk(
        amr_bytes: &[u8],
        message_id: u16,
        bitrate: AmrNbBitrate,
    ) -> Result<Vec<Vec<u8>>, VoiceError> {
        let raw = Self::strip_amr_header(amr_bytes);
        if raw.is_empty() {
            return Err(VoiceError::TooShort { len: 0, needed: 1 });
        }
        let total_chunks_usize = raw.len().div_ceil(MAX_PAYLOAD_SIZE);
        if total_chunks_usize > MAX_CHUNKS_PER_MESSAGE {
            return Err(VoiceError::AudioTooLarge {
                bytes: raw.len(),
                max: MAX_CHUNKS_PER_MESSAGE * MAX_PAYLOAD_SIZE,
            });
        }
        let total = total_chunks_usize as u8;
        let mut out = Vec::with_capacity(total_chunks_usize);
        for (idx, slice) in raw.chunks(MAX_PAYLOAD_SIZE).enumerate() {
            let header = Self::build_header(message_id, idx as u8, total, bitrate);
            let mut packet = Vec::with_capacity(HEADER_SIZE + slice.len());
            packet.extend_from_slice(&header);
            packet.extend_from_slice(slice);
            out.push(packet);
        }
        Ok(out)
    }
}

/// Per-message in-progress reassembly state.
struct AssemblyState {
    total_chunks: u8,
    bitrate: AmrNbBitrate,
    chunks: Vec<Option<Vec<u8>>>,
    received: u8,
    bytes: usize,
    started_at: Instant,
    first_seen: chrono::DateTime<chrono::Utc>,
    to: String,
    channel: u32,
}

impl AssemblyState {
    fn new(total_chunks: u8, bitrate: AmrNbBitrate, to: String, channel: u32) -> Self {
        Self {
            total_chunks,
            bitrate,
            chunks: vec![None; total_chunks as usize],
            received: 0,
            bytes: 0,
            started_at: Instant::now(),
            first_seen: chrono::Utc::now(),
            to,
            channel,
        }
    }
}

/// Outcome of feeding a chunk to the assembler.
#[derive(Debug)]
pub enum AssemblyEvent {
    /// Chunk accepted, message still in progress.
    Pending,
    /// Chunk rejected (already-completed blacklist or duplicate-after-finalize).
    Rejected,
    /// Chunk was a duplicate of one already stored — silently ignored.
    Duplicate,
    /// Message is now complete; reassembled `VoiceMessage` returned.
    Complete(Box<VoiceMessage>),
}

/// Reassembles voice messages from inbound chunks.
///
/// Tracks in-progress messages keyed by `(sender_node_id, message_id)`.
/// Out-of-order, duplicate, and lost chunks are all handled per
/// `VOICE_PROTOCOL.md`:
/// - duplicates within an assembly are silently ignored;
/// - missing chunks are filled with AMR NO_DATA frames on finalize;
/// - finalized message keys go on a 60 s blacklist (max 100 entries).
pub struct VoiceAssembler {
    inner: Mutex<AssemblerInner>,
    timeout: Duration,
    partial_on_timeout: bool,
}

struct AssemblerInner {
    in_progress: HashMap<(String, u16), AssemblyState>,
    blacklist: Vec<((String, u16), Instant)>,
}

impl VoiceAssembler {
    pub fn new(config: &VoiceConfig) -> Self {
        Self {
            inner: Mutex::new(AssemblerInner {
                in_progress: HashMap::new(),
                blacklist: Vec::new(),
            }),
            timeout: Duration::from_secs(config.chunk_timeout_seconds as u64),
            partial_on_timeout: config.partial_play_on_timeout,
        }
    }

    /// Feed a chunk into the assembler.
    pub fn accept(&self, from: &str, to: &str, channel: u32, chunk: VoiceChunk) -> AssemblyEvent {
        let key = (from.to_string(), chunk.message_id);
        let mut inner = self.inner.lock();

        // Drop blacklist entries past TTL while we're here.
        let now = Instant::now();
        inner
            .blacklist
            .retain(|(_, t)| now.duration_since(*t) < BLACKLIST_TTL);
        if inner.blacklist.iter().any(|(k, _)| *k == key) {
            return AssemblyEvent::Rejected;
        }

        // Cap concurrent in-progress reassemblies. If full and this is a new
        // message, evict the oldest one (DoS guard against a peer that emits
        // unique message_ids without ever finishing). Existing entries pass.
        if !inner.in_progress.contains_key(&key)
            && inner.in_progress.len() >= MAX_IN_PROGRESS
            && let Some(victim) = inner
                .in_progress
                .iter()
                .min_by_key(|(_, v)| v.started_at)
                .map(|(k, _)| k.clone())
        {
            warn!(
                from = %victim.0,
                message_id = victim.1,
                in_progress = inner.in_progress.len(),
                "voice assembler full; evicting oldest in-progress message"
            );
            inner.in_progress.remove(&victim);
            push_blacklist(&mut inner.blacklist, victim, now);
        }

        let state = inner.in_progress.entry(key.clone()).or_insert_with(|| {
            AssemblyState::new(chunk.total_chunks, chunk.bitrate, to.to_string(), channel)
        });

        // Same (from, message_id) but a different totalChunks declared.
        // Per protocol the value is fixed for the lifetime of one message;
        // log so a buggy sender doesn't fail silently.
        if chunk.total_chunks != state.total_chunks {
            warn!(
                from = %key.0,
                message_id = key.1,
                expected_total = state.total_chunks,
                got_total = chunk.total_chunks,
                "voice chunk totalChunks mismatch; rejecting"
            );
            return AssemblyEvent::Rejected;
        }

        let idx = chunk.chunk_index as usize;
        if idx >= state.chunks.len() {
            // totalChunks mismatch between chunks of the same message — bail.
            return AssemblyEvent::Rejected;
        }
        if state.chunks[idx].is_some() {
            return AssemblyEvent::Duplicate;
        }
        // Hard byte cap: refuse to grow beyond MAX_MESSAGE_BYTES.
        let new_bytes = state.bytes.saturating_add(chunk.payload.len());
        if new_bytes > MAX_MESSAGE_BYTES {
            warn!(
                from = %key.0,
                message_id = key.1,
                bytes = new_bytes,
                max = MAX_MESSAGE_BYTES,
                "voice message exceeds byte cap; dropping reassembly"
            );
            inner.in_progress.remove(&key);
            push_blacklist(&mut inner.blacklist, key, now);
            return AssemblyEvent::Rejected;
        }
        state.bytes = new_bytes;
        state.chunks[idx] = Some(chunk.payload);
        state.received = state.received.saturating_add(1);

        if state.received == state.total_chunks {
            let state = inner.in_progress.remove(&key).expect("just inserted");
            push_blacklist(&mut inner.blacklist, key.clone(), now);
            let msg = finalize(from, key.1, &state, true);
            return AssemblyEvent::Complete(Box::new(msg));
        }
        AssemblyEvent::Pending
    }

    /// Sweep timed-out in-progress messages. Returns any messages that
    /// finalized as a result (subject to `partial_play_on_timeout`).
    pub fn tick(&self) -> Vec<VoiceMessage> {
        let mut inner = self.inner.lock();
        let now = Instant::now();
        inner
            .blacklist
            .retain(|(_, t)| now.duration_since(*t) < BLACKLIST_TTL);

        let timeout = self.timeout;
        let expired_keys: Vec<(String, u16)> = inner
            .in_progress
            .iter()
            .filter(|(_, v)| now.duration_since(v.started_at) >= timeout)
            .map(|(k, _)| k.clone())
            .collect();

        let mut out = Vec::new();
        for key in expired_keys {
            let state = inner.in_progress.remove(&key).expect("just listed");
            push_blacklist(&mut inner.blacklist, key.clone(), now);
            if self.partial_on_timeout {
                out.push(finalize(&key.0, key.1, &state, false));
            }
        }
        out
    }
}

fn push_blacklist(bl: &mut Vec<((String, u16), Instant)>, key: (String, u16), now: Instant) {
    bl.push((key, now));
    if bl.len() > BLACKLIST_MAX {
        let drop = bl.len() - BLACKLIST_MAX;
        bl.drain(0..drop);
    }
}

fn finalize(from: &str, message_id: u16, state: &AssemblyState, complete: bool) -> VoiceMessage {
    // For each missing chunk, substitute AMR NO_DATA frames sized so the audio
    // timeline stays aligned (`floor(MAX_PAYLOAD_SIZE / frame_size)` frames per
    // chunk, each one byte = `0x7C`).
    let frames_per_chunk = MAX_PAYLOAD_SIZE / state.bitrate.frame_size();
    let mut audio =
        Vec::with_capacity(AMR_FILE_HEADER.len() + state.chunks.len() * MAX_PAYLOAD_SIZE);
    audio.extend_from_slice(AMR_FILE_HEADER);
    for slot in &state.chunks {
        match slot {
            Some(payload) => audio.extend_from_slice(payload),
            None => {
                audio.resize(audio.len() + frames_per_chunk, AMR_NO_DATA_FRAME);
            }
        }
    }

    VoiceMessage {
        message_id,
        from: from.to_string(),
        to: state.to.clone(),
        audio_data: audio,
        timestamp: state.first_seen,
        is_outgoing: false,
        is_complete: complete,
        total_chunks: state.total_chunks,
        received_chunks: state.received,
        bitrate: state.bitrate,
        channel: state.channel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_amr(n: usize) -> Vec<u8> {
        // Synthetic payload — the chunker doesn't care that it's not real AMR.
        let mut v = Vec::with_capacity(AMR_FILE_HEADER.len() + n);
        v.extend_from_slice(AMR_FILE_HEADER);
        v.extend((0..n).map(|i| (i & 0xff) as u8));
        v
    }

    #[test]
    fn chunker_strips_header_and_caps_at_225() {
        let src = raw_amr(500);
        let chunks = VoiceChunker::chunk(&src, 42, AmrNbBitrate::Mr795).unwrap();
        // 500 bytes raw / 225 = 3 chunks (225 + 225 + 50)
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.len() <= MAX_PACKET_SIZE));
        assert_eq!(chunks[0].len(), HEADER_SIZE + 225);
        assert_eq!(chunks[2].len(), HEADER_SIZE + 50);
        // Header layout
        assert_eq!(chunks[0][0], PROTOCOL_VERSION);
        assert_eq!(u16::from_be_bytes([chunks[0][1], chunks[0][2]]), 42);
        assert_eq!(chunks[0][3], 0);
        assert_eq!(chunks[1][3], 1);
        assert_eq!(chunks[2][3], 2);
        assert!(chunks.iter().all(|c| c[4] == 3));
        assert!(chunks.iter().all(|c| c[5] == AmrNbBitrate::Mr795 as u8));
    }

    #[test]
    fn chunker_works_without_amr_header() {
        let src: Vec<u8> = (0..100).collect();
        let chunks = VoiceChunker::chunk(&src, 1, AmrNbBitrate::Mr475).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(&chunks[0][HEADER_SIZE..], &src[..]);
    }

    #[test]
    fn chunker_rejects_too_large_audio() {
        // Need > 255 * 225 raw bytes to overflow.
        let too_big = vec![0u8; 256 * MAX_PAYLOAD_SIZE];
        assert!(matches!(
            VoiceChunker::chunk(&too_big, 0, AmrNbBitrate::Mr795),
            Err(VoiceError::AudioTooLarge { .. })
        ));
    }

    #[test]
    fn parse_rejects_bad_version() {
        let mut pkt = vec![2u8, 0, 0, 0, 1, 5];
        pkt.extend_from_slice(b"hi");
        assert!(matches!(
            VoiceChunk::parse(&pkt),
            Err(VoiceError::BadVersion(2))
        ));
    }

    #[test]
    fn parse_rejects_bad_bitrate() {
        let pkt = [PROTOCOL_VERSION, 0, 0, 0, 1, 99];
        assert!(matches!(
            VoiceChunk::parse(&pkt),
            Err(VoiceError::BadBitrate(99))
        ));
    }

    #[test]
    fn parse_rejects_index_out_of_range() {
        let pkt = [PROTOCOL_VERSION, 0, 0, 5, 3, 5];
        assert!(matches!(
            VoiceChunk::parse(&pkt),
            Err(VoiceError::BadIndex { .. })
        ));
    }

    #[test]
    fn assembler_round_trip_in_order() {
        let cfg = VoiceConfig::default();
        let asm = VoiceAssembler::new(&cfg);
        let src = raw_amr(450);
        let pkts = VoiceChunker::chunk(&src, 7, AmrNbBitrate::Mr795).unwrap();
        let mut completed = None;
        for p in &pkts {
            let chunk = VoiceChunk::parse(p).unwrap();
            match asm.accept("!a1b2c3d4", "broadcast", 0, chunk) {
                AssemblyEvent::Complete(m) => completed = Some(m),
                AssemblyEvent::Pending => {}
                e => panic!("unexpected: {e:?}"),
            }
        }
        let m = completed.expect("complete");
        assert!(m.is_complete);
        assert!(m.audio_data.starts_with(AMR_FILE_HEADER));
        assert_eq!(
            &m.audio_data[AMR_FILE_HEADER.len()..],
            &src[AMR_FILE_HEADER.len()..]
        );
    }

    #[test]
    fn assembler_handles_out_of_order_and_dupes() {
        let cfg = VoiceConfig::default();
        let asm = VoiceAssembler::new(&cfg);
        let src = raw_amr(700);
        let pkts = VoiceChunker::chunk(&src, 3, AmrNbBitrate::Mr795).unwrap();
        // Send in reverse, then re-send chunk 0 as a duplicate.
        let mut completed = None;
        for p in pkts.iter().rev() {
            let chunk = VoiceChunk::parse(p).unwrap();
            match asm.accept("!cafebabe", "broadcast", 1, chunk) {
                AssemblyEvent::Complete(m) => completed = Some(m),
                AssemblyEvent::Pending => {}
                e => panic!("unexpected: {e:?}"),
            }
        }
        let dup = VoiceChunk::parse(&pkts[0]).unwrap();
        // Already finalized → blacklisted.
        assert!(matches!(
            asm.accept("!cafebabe", "broadcast", 1, dup),
            AssemblyEvent::Rejected
        ));
        let m = completed.expect("complete");
        assert_eq!(
            &m.audio_data[AMR_FILE_HEADER.len()..],
            &src[AMR_FILE_HEADER.len()..]
        );
    }

    #[test]
    fn assembler_dedups_within_message() {
        let cfg = VoiceConfig::default();
        let asm = VoiceAssembler::new(&cfg);
        let src = raw_amr(700);
        let pkts = VoiceChunker::chunk(&src, 9, AmrNbBitrate::Mr795).unwrap();
        let c0 = VoiceChunk::parse(&pkts[0]).unwrap();
        assert!(matches!(
            asm.accept("!11111111", "broadcast", 0, c0),
            AssemblyEvent::Pending
        ));
        let c0_dup = VoiceChunk::parse(&pkts[0]).unwrap();
        assert!(matches!(
            asm.accept("!11111111", "broadcast", 0, c0_dup),
            AssemblyEvent::Duplicate
        ));
    }

    #[test]
    fn assembler_fills_missing_with_no_data() {
        // Build a 2-chunk message; deliver only chunk 1; force timeout via tick().
        let cfg = VoiceConfig {
            chunk_timeout_seconds: 0,
            partial_play_on_timeout: true,
            ..Default::default()
        };
        let asm = VoiceAssembler::new(&cfg);
        let src = raw_amr(300);
        let pkts = VoiceChunker::chunk(&src, 4, AmrNbBitrate::Mr795).unwrap();
        let c1 = VoiceChunk::parse(&pkts[1]).unwrap();
        assert!(matches!(
            asm.accept("!deadbeef", "broadcast", 0, c1),
            AssemblyEvent::Pending
        ));
        // Timeout sweep
        std::thread::sleep(Duration::from_millis(5));
        let out = asm.tick();
        assert_eq!(out.len(), 1);
        let m = &out[0];
        assert!(!m.is_complete);
        // The first slot was missing → frames_per_chunk NO_DATA bytes (each 0x7C).
        let frames_per_chunk = MAX_PAYLOAD_SIZE / AmrNbBitrate::Mr795.frame_size();
        let after_header = &m.audio_data[AMR_FILE_HEADER.len()..];
        assert!(
            after_header
                .iter()
                .take(frames_per_chunk)
                .all(|b| *b == AMR_NO_DATA_FRAME)
        );
    }

    #[test]
    fn assembler_discards_partial_when_disabled() {
        let cfg = VoiceConfig {
            chunk_timeout_seconds: 0,
            partial_play_on_timeout: false,
            ..Default::default()
        };
        let asm = VoiceAssembler::new(&cfg);
        let src = raw_amr(300);
        let pkts = VoiceChunker::chunk(&src, 4, AmrNbBitrate::Mr795).unwrap();
        let c0 = VoiceChunk::parse(&pkts[0]).unwrap();
        let _ = asm.accept("!deadbeef", "broadcast", 0, c0);
        std::thread::sleep(Duration::from_millis(5));
        assert!(asm.tick().is_empty());
    }

    #[test]
    fn frame_sizes_match_spec() {
        use AmrNbBitrate::*;
        for (b, s) in [
            (Mr475, 13),
            (Mr515, 14),
            (Mr59, 16),
            (Mr67, 18),
            (Mr74, 20),
            (Mr795, 21),
            (Mr102, 27),
            (Mr122, 32),
        ] {
            assert_eq!(b.frame_size(), s);
        }
    }
}
