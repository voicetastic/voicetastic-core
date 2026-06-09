//! Tunable [`AssemblerConfig`] for the receive-side state machine.

use std::time::Duration;

use super::super::consts::{DEAD_SENDER_TIMEOUT, NACK_MAX_ROUNDS, NACK_WINDOW_MS};
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
    /// Maximum NACK rounds before the receiver finalizes (with whatever
    /// chunks it has). Each round fires after `nack_window` of silence.
    pub max_nack_rounds: u16,
    /// Quiet-period after the last seen chunk before issuing a NACK round.
    pub nack_window: Duration,
    /// Base of the per-round exponential backoff: effective quiet window
    /// for round `n` is `nack_window × backoff_base.pow(min(n, 4))`.
    /// `2` doubles, `3` triples. **Special value `0`** disables NACK
    /// emission entirely — the assembler skips the emission branch. Used
    /// by the `Off` setting and as a hard override for broadcast messages.
    pub nack_backoff_base: u32,
    /// How long a `(from, message_id)` pair is remembered as "already
    /// completed" after the receiver finalizes it. Late chunks for that
    /// pair are silently dropped within this window so the sender's
    /// firmware-queue drain (which can outrun the receiver's completion
    /// by tens of seconds on slow presets) doesn't resurrect a phantom
    /// partial reassembly.
    pub completion_memory: Duration,
    /// If no real data (data/parity chunk) arrives within this window
    /// the sender is presumed dead and NACKs are suppressed until the
    /// hard `message_timeout` fires. Prevents a long tail of sparse
    /// cap-multiplied NACKs after the sender has dropped off the mesh.
    pub dead_sender_timeout: Duration,
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
        // 15 minutes: at ~3 s/frame on LongFast with max chunk_size 199,
        // even worst-case 250-frame bursts leave ~50 s margin. Raised to
        // 1200 s (20 min) so a LongFast voice message that sits at the
        // firmware queue full (bp_ms ≈ 2-3.6 s) can complete its initial
        // burst and still service several NACK recovery rounds.
        let message_timeout = Duration::from_secs(1200);
        Self {
            message_timeout,
            partial_play_on_timeout: true,
            // Allow many NACK rounds: with a 1.5 s window that's ~3 min of
            // total silence before we stop trying. The hard message_timeout
            // is the real ceiling.
            max_nack_rounds: NACK_MAX_ROUNDS,
            nack_window: Duration::from_millis(NACK_WINDOW_MS),
            // Default to tripling per round, matching the historical
            // behaviour before `nack_backoff_base` became configurable.
            nack_backoff_base: 3,
            dead_sender_timeout: DEAD_SENDER_TIMEOUT,
            // completion_memory must be >= message_timeout so that late
            // chunks from the sender's retain_ttl don't create a fresh
            // assembly entry (and NACK storm) for an already-finalized
            // message while the sender still has chunks in flight.
            completion_memory: message_timeout,
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
        // completion_memory must outlive message_timeout so the blacklist
        // stays armed until the sender's firmware queue fully drains (the
        // sender's retain_ttl is tied to message_timeout). Without this, a
        // late chunk arriving after the blacklist expires would resurrect
        // the assembly slot and restart the NACK storm.
        self.completion_memory = self.completion_memory.max(self.message_timeout);
    }

    /// Validate config invariants. Returns `Ok(())` if valid, else a descriptive error.
    pub fn validate(&self) -> Result<(), String> {
        if self.message_timeout.is_zero() {
            return Err("message_timeout must be > 0".to_string());
        }
        if self.dead_sender_timeout.is_zero() {
            return Err("dead_sender_timeout must be > 0".to_string());
        }
        if self.dead_sender_timeout >= self.message_timeout {
            return Err(format!(
                "dead_sender_timeout ({:?}) must be < message_timeout ({:?})",
                self.dead_sender_timeout, self.message_timeout
            ));
        }
        if self.nack_window.is_zero() {
            return Err("nack_window must be > 0".to_string());
        }
        // completion_memory must outlive message_timeout: the completion
        // blacklist has to stay armed until the sender's retransmit tail
        // (tied to message_timeout) fully drains. A shorter window prunes the
        // blacklist while late chunks are still arriving, which resurrects the
        // finalized slot and restarts the NACK storm. `sync_nack_cap_to_timeout`
        // enforces this by construction; this guards hosts that set the fields
        // directly.
        if self.completion_memory < self.message_timeout {
            return Err(format!(
                "completion_memory ({:?}) must be >= message_timeout ({:?})",
                self.completion_memory, self.message_timeout
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_nack_cap_default() {
        // Default 1200 s timeout / 3000 ms (3 s) window = 400 rounds.
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
        // 3600 s slider max / default 3000 ms (3 s) = 1200 rounds.
        let mut cfg = AssemblerConfig {
            message_timeout: Duration::from_secs(3_600),
            ..Default::default()
        };
        cfg.sync_nack_cap_to_timeout();
        assert_eq!(cfg.max_nack_rounds, 1_200);
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

    #[test]
    fn validate_accepts_default() {
        assert!(AssemblerConfig::default().validate().is_ok());
    }

    #[test]
    fn validate_rejects_completion_memory_below_timeout() {
        // A blacklist window shorter than the message timeout would let a
        // finalized message be resurrected by late chunks (NACK storm).
        let cfg = AssemblerConfig {
            message_timeout: Duration::from_secs(1200),
            completion_memory: Duration::from_secs(10),
            ..Default::default()
        };
        let err = cfg
            .validate()
            .expect_err("must reject short completion_memory");
        assert!(err.contains("completion_memory"), "unexpected error: {err}");
    }

    #[test]
    fn validate_accepts_completion_memory_equal_to_timeout() {
        let cfg = AssemblerConfig {
            message_timeout: Duration::from_secs(300),
            completion_memory: Duration::from_secs(300),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }
}
