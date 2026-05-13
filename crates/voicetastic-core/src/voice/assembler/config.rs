//! Tunable [`AssemblerConfig`] for the receive-side state machine.

use std::time::Duration;

use super::super::consts::{BLACKLIST_TTL, NACK_MAX_ROUNDS, NACK_WINDOW_MS};
use super::super::types::VoiceCodec;

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
    pub max_nack_rounds: u16,
    /// Quiet-period after the last seen chunk before issuing a NACK round.
    pub nack_window: Duration,
    /// How long a `(from, message_id)` pair is remembered as "already
    /// completed" after the receiver finalizes it. Late chunks for that
    /// pair are silently dropped within this window so the sender's
    /// firmware-queue drain (which can outrun the receiver's completion
    /// by tens of seconds on slow presets) doesn't resurrect a phantom
    /// partial reassembly.
    pub completion_memory: Duration,
    /// Codecs the local stack can actually decode. Frames whose header
    /// advertises a codec outside this set are rejected with
    /// [`super::super::error::VoiceError::UnsupportedCodec`] before any
    /// reassembly state is allocated, so an Opus-only build won't waste
    /// a per-sender slot trying to reassemble an AMR-NB message it can
    /// never play back.
    ///
    /// `None` (the default) means "accept any known codec" — i.e. the
    /// pre-existing behaviour. Callers that know their playback layer is
    /// restricted (CLI's `voice send` is AMR-NB only, GUI without the
    /// `audio` feature has no decoders at all) should populate this.
    pub supported_codecs: Option<Vec<VoiceCodec>>,
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
            supported_codecs: None,
        }
    }
}

impl AssemblerConfig {
    /// Recompute [`Self::max_nack_rounds`] so the consecutive-silence
    /// budget (`max_nack_rounds × nack_window`) covers
    /// [`Self::message_timeout`]. Call after mutating either field
    /// from a host-driven setting (GUI slider, CLI flag, LoRa preset
    /// change) so the user-configured per-message timeout — not the
    /// round cap — is the practical ceiling.
    ///
    /// Tests that want a deterministic round cap (e.g.
    /// `silent_sender_partial_finalizes_after_cap`) should set
    /// `max_nack_rounds` *after* this call, or skip it entirely.
    pub fn sync_nack_cap_to_timeout(&mut self) {
        let window_ms = self.nack_window.as_millis().max(1) as u64;
        let timeout_ms = self.message_timeout.as_millis() as u64;
        let derived = timeout_ms.div_ceil(window_ms).min(u64::from(u16::MAX)) as u16;
        self.max_nack_rounds = derived;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_nack_cap_default() {
        // Default 600 s timeout / 1500 ms window = 400 rounds.
        let mut cfg = AssemblerConfig::default();
        cfg.sync_nack_cap_to_timeout();
        assert_eq!(cfg.max_nack_rounds, 400);
    }

    #[test]
    fn sync_nack_cap_round_up() {
        // 5 s / 2 s = 2.5 → ceil = 3.
        let mut cfg = AssemblerConfig {
            message_timeout: Duration::from_secs(5),
            nack_window: Duration::from_secs(2),
            ..Default::default()
        };
        cfg.sync_nack_cap_to_timeout();
        assert_eq!(cfg.max_nack_rounds, 3);
    }

    #[test]
    fn sync_nack_cap_top_of_slider() {
        // 3600 s slider max / default 1500 ms = 2400 rounds.
        let mut cfg = AssemblerConfig {
            message_timeout: Duration::from_secs(3_600),
            ..Default::default()
        };
        cfg.sync_nack_cap_to_timeout();
        assert_eq!(cfg.max_nack_rounds, 2_400);
    }

    #[test]
    fn sync_nack_cap_saturates_to_u16() {
        // Pathological: huge timeout, tiny window → would overflow u16,
        // must clamp instead of wrapping.
        let mut cfg = AssemblerConfig {
            message_timeout: Duration::from_secs(86_400),
            nack_window: Duration::from_millis(1),
            ..Default::default()
        };
        cfg.sync_nack_cap_to_timeout();
        assert_eq!(cfg.max_nack_rounds, u16::MAX);
    }

    #[test]
    fn sync_nack_cap_zero_window_is_safe() {
        // Misconfigured zero window must not divide-by-zero; the impl
        // floors window to 1 ms so the cap is `timeout_ms / 1`.
        let mut cfg = AssemblerConfig {
            message_timeout: Duration::from_secs(60),
            nack_window: Duration::ZERO,
            ..Default::default()
        };
        cfg.sync_nack_cap_to_timeout();
        // 60_000 ms / 1 ms = 60_000, capped at u16::MAX.
        assert_eq!(cfg.max_nack_rounds, 60_000);
    }
}
