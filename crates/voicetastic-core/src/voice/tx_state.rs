//! Sans-IO state machine for sending one voice burst, frame by frame.
//!
//! ## Scope
//!
//! This is the **sans-IO** TX path, intended for drivers that can't use
//! tokio primitives directly — primarily the future `wasm32` browser
//! client, which paces frames on `setTimeout` instead of
//! `tokio::time::sleep`. It's published as part of the crate's public
//! API so a non-native driver can drive the protocol through pure
//! `next_action(now, queue_free)` calls without naming any tokio types.
//!
//! The native (tokio) path lives in
//! [`crate::meshtastic::service::voice_tx`] and is not built on top of
//! `VoiceTx` — it shares only the pure pacing/backpressure policy in
//! [`crate::voice::tx_policy`]. Both paths converge on identical
//! per-frame airtime gap + firmware-queue-depth decisions; only the
//! waiting primitive differs.
//!
//! Usage from a driver:
//!
//! ```ignore
//! let mut tx = VoiceTx::new(prepared.total_data, prepared.frames,
//!                           channel, to, preset.pacing());
//! loop {
//!     match tx.next_action(Instant::now(), queue_free.get()) {
//!         VoiceTxAction::Send { frame, is_data, chunk_index, .. } => {
//!             write_to_radio(frame).await?;
//!             if is_data {
//!                 registry.mark_chunk_sent(message_id, chunk_index);
//!             }
//!         }
//!         VoiceTxAction::Wait(d) => sleep(d).await,
//!         VoiceTxAction::Done => break,
//!     }
//! }
//! ```
//!
//! The driver's only responsibilities:
//! - track its own clock (pass `Instant::now()` to `next_action`)
//! - track the firmware's `QueueStatus.free` (pass it to `next_action`)
//! - actually write frames to the radio
//! - mark DATA chunks on the [`crate::voice::OutgoingVoiceRegistry`] so
//!   future NACK rounds find them
//!
//! The state machine handles pacing, queue backpressure, the
//! [`crate::voice::tx_policy::RADIO_QUEUE_WAIT_TIMEOUT`] safety valve,
//! and frame ordering.

use std::collections::VecDeque;
use std::time::Duration;

use web_time::Instant;

use crate::voice::tx_policy::{self, RADIO_QUEUE_WAIT_TIMEOUT};

/// How often to re-check queue depth while backpressured. Driver waits
/// this long between `next_action` calls when the action is `Wait` due
/// to a full radio queue; on each new call it provides the latest
/// `queue_free`, which the firmware updates via `QueueStatus`. Short
/// enough to feel responsive, long enough not to spam the event loop.
const QUEUE_RECHECK: Duration = Duration::from_millis(60);

/// One voice burst's worth of frames, in send order, with the pacing /
/// backpressure state for the loop. Created from
/// [`crate::voice::EncodedMessage::frames`] (typically via
/// [`super::send_prep::prepare_voice_send`]).
#[derive(Debug)]
pub struct VoiceTx {
    /// Number of DATA chunks (frames `0..total_data`). Frames at or after
    /// this index are PARITY and the registry doesn't track them.
    total_data: u8,
    /// Frames left to send. Front = next out. Each entry is `(chunk_index, body)`.
    queue: VecDeque<(u8, Vec<u8>)>,
    /// LoRa channel index for every frame in this burst.
    channel: u32,
    /// Destination — `None` = broadcast, `Some(node)` = DM with want_ack.
    to: Option<u32>,
    /// Per-frame airtime gap derived from the radio's modem preset.
    pacing: Duration,
    /// When we last handed a frame to the driver. `None` before the first
    /// send. Used for pacing math via [`tx_policy::pacing_delay`].
    last_send_at: Option<Instant>,
    /// First `now` we observed while the queue was full. Reset whenever
    /// the queue has room. Drives the
    /// [`RADIO_QUEUE_WAIT_TIMEOUT`] safety valve — if the firmware stops
    /// reporting queue updates we proceed anyway after the timeout.
    queue_block_started_at: Option<Instant>,
}

/// One step the driver should take after consulting [`VoiceTx::next_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceTxAction {
    /// Send this frame now. The state has already advanced — the frame is
    /// no longer in the queue. After a successful write, the driver
    /// updates its `last_send` clock for the next call.
    Send {
        /// Index of the chunk within the burst, matching `ChunkHeader.chunk_index`.
        chunk_index: u8,
        /// Frame body (header + payload), ready to be wrapped by the
        /// transport's `data_packet` builder.
        frame: Vec<u8>,
        channel: u32,
        to: Option<u32>,
        /// Whether to set `want_ack` on the outgoing data packet.
        /// True for DMs (matches the native voice TX worker).
        want_ack: bool,
        /// `true` when `chunk_index < total_data` — the driver should call
        /// [`crate::voice::OutgoingVoiceRegistry::mark_chunk_sent`] so
        /// future NACK rounds can request this chunk again if it gets lost.
        /// Parity frames aren't tracked by the registry.
        is_data: bool,
    },
    /// Pace / backpressure not satisfied — wait this long, then call
    /// [`VoiceTx::next_action`] again with a fresh `now` and the latest
    /// `queue_free` count.
    Wait(Duration),
    /// All frames sent — the driver may drop the state.
    Done,
}

impl VoiceTx {
    /// Build a state machine for one burst. `frames` is expected in the
    /// order produced by [`crate::voice::build_message`] (DATA chunks
    /// 0..total_data first, then PARITY 0..parity_count).
    pub fn new(
        total_data: u8,
        frames: Vec<(u8, Vec<u8>)>,
        channel: u32,
        to: Option<u32>,
        pacing: Duration,
    ) -> Self {
        Self {
            total_data,
            queue: frames.into(),
            channel,
            to,
            pacing,
            last_send_at: None,
            queue_block_started_at: None,
        }
    }

    /// Decide what to do next given the current clock + the firmware's
    /// last reported queue depth. Mutating: a `Send` return pops the
    /// front frame off the queue.
    pub fn next_action(&mut self, now: Instant, queue_free: u32) -> VoiceTxAction {
        if self.queue.is_empty() {
            return VoiceTxAction::Done;
        }

        // Pacing: wait until the inter-frame gap has elapsed.
        let elapsed = self.last_send_at.map(|t| now.duration_since(t));
        let pace_wait = tx_policy::pacing_delay(elapsed, self.pacing);
        if !pace_wait.is_zero() {
            return VoiceTxAction::Wait(pace_wait);
        }

        // Backpressure: pause until the firmware drains. The safety
        // valve (`RADIO_QUEUE_WAIT_TIMEOUT`) covers the case where the
        // firmware never publishes another `QueueStatus`.
        if !tx_policy::queue_has_room(queue_free) {
            let blocked_since = *self.queue_block_started_at.get_or_insert(now);
            if now.duration_since(blocked_since) < RADIO_QUEUE_WAIT_TIMEOUT {
                return VoiceTxAction::Wait(QUEUE_RECHECK);
            }
            // Timeout reached — fall through and send anyway. Reset so the
            // next stall starts a fresh countdown.
            self.queue_block_started_at = None;
        } else {
            self.queue_block_started_at = None;
        }

        // Pop the front frame and emit Send. last_send_at advances now —
        // pacing math for the next call uses this as t=0 even if the
        // write hasn't actually happened yet. Same convention as the
        // native worker.
        let (chunk_index, frame) = self
            .queue
            .pop_front()
            .expect("queue non-empty per the early-return above");
        self.last_send_at = Some(now);
        VoiceTxAction::Send {
            chunk_index,
            frame,
            channel: self.channel,
            to: self.to,
            want_ack: self.to.is_some(),
            is_data: chunk_index < self.total_data,
        }
    }

    /// Frames not yet emitted via `Send`. Useful for progress UI / tests.
    pub fn frames_remaining(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(n: u8) -> Vec<(u8, Vec<u8>)> {
        (0..n).map(|i| (i, vec![i, i, i, i])).collect()
    }

    #[test]
    fn empty_queue_is_done_immediately() {
        let mut tx = VoiceTx::new(0, vec![], 0, None, Duration::from_millis(900));
        assert_eq!(tx.next_action(Instant::now(), 16), VoiceTxAction::Done,);
    }

    #[test]
    fn first_frame_sends_immediately() {
        let mut tx = VoiceTx::new(3, frames(3), 0, None, Duration::from_millis(900));
        match tx.next_action(Instant::now(), 16) {
            VoiceTxAction::Send {
                chunk_index,
                is_data,
                want_ack,
                ..
            } => {
                assert_eq!(chunk_index, 0);
                assert!(is_data);
                assert!(!want_ack, "broadcast must not set want_ack");
            }
            other => panic!("expected Send, got {other:?}"),
        }
        assert_eq!(tx.frames_remaining(), 2);
    }

    #[test]
    fn dm_sets_want_ack() {
        let mut tx = VoiceTx::new(1, frames(1), 0, Some(0x42), Duration::from_millis(900));
        match tx.next_action(Instant::now(), 16) {
            VoiceTxAction::Send { want_ack, .. } => assert!(want_ack),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn parity_frames_are_not_marked_data() {
        // total_data=2, but we hand 3 frames — index 2 is parity.
        let mut tx = VoiceTx::new(2, frames(3), 0, None, Duration::ZERO);
        let now = Instant::now();
        for expected_is_data in [true, true, false] {
            match tx.next_action(now, 16) {
                VoiceTxAction::Send { is_data, .. } => assert_eq!(is_data, expected_is_data),
                other => panic!("expected Send, got {other:?}"),
            }
        }
        assert_eq!(tx.next_action(now, 16), VoiceTxAction::Done);
    }

    #[test]
    fn pacing_makes_us_wait_between_frames() {
        let pacing = Duration::from_millis(900);
        let mut tx = VoiceTx::new(2, frames(2), 0, None, pacing);
        let t0 = Instant::now();
        // First frame: send.
        assert!(matches!(tx.next_action(t0, 16), VoiceTxAction::Send { .. }));
        // Only 300 ms later → wait the remaining 600.
        let t1 = t0 + Duration::from_millis(300);
        assert_eq!(
            tx.next_action(t1, 16),
            VoiceTxAction::Wait(Duration::from_millis(600)),
        );
        // Full gap elapsed → second frame sends.
        let t2 = t0 + pacing;
        assert!(matches!(tx.next_action(t2, 16), VoiceTxAction::Send { .. }));
        assert_eq!(tx.next_action(t2, 16), VoiceTxAction::Done);
    }

    #[test]
    fn backpressure_returns_wait_when_queue_full() {
        let mut tx = VoiceTx::new(1, frames(1), 0, None, Duration::ZERO);
        // queue_free of 0 → wait. Pacing is zero so pacing isn't the cause.
        match tx.next_action(Instant::now(), 0) {
            VoiceTxAction::Wait(d) => assert_eq!(d, QUEUE_RECHECK),
            other => panic!("expected Wait, got {other:?}"),
        }
        assert_eq!(tx.frames_remaining(), 1, "no frame consumed while blocked");
    }

    #[test]
    fn backpressure_safety_valve_proceeds_after_timeout() {
        let mut tx = VoiceTx::new(1, frames(1), 0, None, Duration::ZERO);
        let t0 = Instant::now();
        // First call records the stall start.
        assert!(matches!(tx.next_action(t0, 0), VoiceTxAction::Wait(_)));
        // Halfway through the timeout — still waiting.
        let t_half = t0 + RADIO_QUEUE_WAIT_TIMEOUT / 2;
        assert!(matches!(tx.next_action(t_half, 0), VoiceTxAction::Wait(_)));
        // Past the timeout — send anyway.
        let t_after = t0 + RADIO_QUEUE_WAIT_TIMEOUT + Duration::from_millis(10);
        assert!(matches!(
            tx.next_action(t_after, 0),
            VoiceTxAction::Send { .. }
        ));
    }

    #[test]
    fn backpressure_resets_when_queue_drains() {
        let mut tx = VoiceTx::new(2, frames(2), 0, None, Duration::ZERO);
        let t0 = Instant::now();
        // Stall.
        assert!(matches!(tx.next_action(t0, 0), VoiceTxAction::Wait(_)));
        // Queue drains — first frame goes out.
        assert!(matches!(tx.next_action(t0, 16), VoiceTxAction::Send { .. }));
        // Stall again — countdown restarts (won't fire for another full timeout).
        let t_late = t0 + RADIO_QUEUE_WAIT_TIMEOUT / 2;
        assert!(matches!(tx.next_action(t_late, 0), VoiceTxAction::Wait(_)));
        // Just past half the original timeout from t0 — but the stall counter
        // restarted at t_late, so we're still waiting.
        let t_late_plus = t_late + RADIO_QUEUE_WAIT_TIMEOUT / 2;
        assert!(matches!(
            tx.next_action(t_late_plus, 0),
            VoiceTxAction::Wait(_)
        ));
    }
}
