//! Tunable [`AssemblerConfig`] for the receive-side state machine.

use std::time::Duration;

use super::super::consts::{BLACKLIST_TTL, NACK_MAX_ROUNDS, NACK_WINDOW_MS};

/// After this many post-template validation failures (codec / total_data /
/// stream_seq mismatch) on the same in-progress entry, the entry is
/// evicted and blacklisted. Keeps a chatty bad sender from holding a
/// per-sender slot for the full message timeout.
pub(super) const MAX_VALIDATION_STRIKES: u8 = 3;

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
    /// Maximum NACK rounds before the receiver finalizes (with whatever
    /// chunks it has). Each round fires after `nack_window` of silence.
    pub max_nack_rounds: u8,
    /// Quiet-period after the last seen chunk before issuing a NACK round.
    pub nack_window: Duration,
    /// How long a `(from, message_id)` pair is remembered as "already
    /// completed" after the receiver finalizes it. Late chunks for that
    /// pair are silently dropped within this window so the sender's
    /// firmware-queue drain (which can outrun the receiver's completion
    /// by tens of seconds on slow presets) doesn't resurrect a phantom
    /// partial reassembly.
    pub completion_memory: Duration,
}

impl Default for AssemblerConfig {
    fn default() -> Self {
        Self {
            // 10 minutes: voice messages may stretch over many seconds on
            // slow modem presets and tolerate long quiet gaps while NACK
            // rounds chase missing chunks. The previous 30 s default was
            // too aggressive for real LoRa links.
            message_timeout: Duration::from_secs(600),
            partial_play_on_timeout: true,
            channel_psk: None,
            // Allow many NACK rounds: with a 1.5 s window that's ~3 min of
            // total silence before we stop trying. The hard message_timeout
            // is the real ceiling.
            max_nack_rounds: NACK_MAX_ROUNDS,
            nack_window: Duration::from_millis(NACK_WINDOW_MS),
            completion_memory: BLACKLIST_TTL,
        }
    }
}
