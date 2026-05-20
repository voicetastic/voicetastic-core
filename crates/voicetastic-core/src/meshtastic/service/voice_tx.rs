//! Serialized voice-frame TX queue.
//!
//! All voice frames — initial sends from `send_voice` and NACK-driven
//! retransmits — funnel through a single worker that paces them with a
//! per-frame minimum inter-send gap. This guarantees we never push two
//! voice frames at the firmware closer together than the modem preset
//! can transmit, which is the failure mode that caused the receiver to
//! see only ~16 % of a 107-chunk burst on real LoRa links.
//!
//! Non-voice traffic (text, admin) is unaffected — it bypasses this
//! queue entirely.
//!
//! ## Head-of-line caveat
//!
//! There is a single worker for the whole service, so a long broadcast
//! voice message blocks any concurrent DM voice traffic behind it. This
//! is fine in practice today (one user, one composer) but worth knowing
//! if multi-stream voice ever becomes a requirement — the fix is a
//! `(channel, dest)`-keyed map of workers.
//!
//! ## Pacing measurement
//!
//! `last_send` is the instant the underlying [`MeshtasticService::send_data`]
//! call returned, i.e. when the transport (BLE / serial) accepted the
//! frame — not when the radio finished transmitting it. The configured
//! `pacing` values include enough headroom (airtime + ~30 %) that the
//! firmware's internal LoRa queue drains before the next hand-off.

use std::sync::Weak;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::error::{Error, Result};
use crate::ports::PRIVATE_APP;

use super::Inner;
use super::MeshtasticService;

/// One voice frame waiting to be sent.
pub(super) struct VoiceTxItem {
    pub(super) frame: Vec<u8>,
    pub(super) channel: u32,
    pub(super) to: Option<u32>,
    pub(super) want_ack: bool,
    /// Minimum gap to enforce *before* sending this frame, measured from
    /// the previous voice frame's send time. The first frame after a
    /// long idle period bypasses the sleep.
    pub(super) pacing: Duration,
    /// Optional one-shot for callers that want the assigned packet id
    /// (or the send error). Drop the receiver to ignore.
    pub(super) done: Option<oneshot::Sender<Result<u32>>>,
}

/// Channel buffer size. With the parity-scaling change a worst-case
/// 255-data-chunk message ships up to 383 frames (data + parity), so
/// the bound is sized to hold one such message in flight while another
/// is being enqueued. Bounded so a runaway producer applies
/// backpressure rather than allocating without limit.
pub(super) const QUEUE_CAPACITY: usize = 512;

/// Firmware queue low-water mark. When the device reports
/// `QueueStatus.free <= RADIO_QUEUE_LOW_WATER` we pause the voice TX
/// worker until the next update. Meshtastic firmware sizes its
/// outbound queue at ~16 slots; leaving a small safety margin prevents
/// us from racing the radio into "queue full" rejections (and the
/// out-of-memory reboots that follow on long voice bursts).
pub(super) const RADIO_QUEUE_LOW_WATER: u32 = 2;

/// Maximum time we wait for a fresh `QueueStatus` notification before
/// proceeding anyway. Acts as a safety valve for the (rare) case where
/// the firmware never publishes another update, e.g. because traffic
/// has stalled. The configured per-frame pacing still provides
/// timer-based throttling underneath.
pub(super) const RADIO_QUEUE_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Spawn the FIFO worker. The worker holds only a `Weak<Inner>` so
/// dropping the last external [`MeshtasticService`] clone tears it down
/// cleanly. The receiver half of the channel is created in
/// [`MeshtasticService::new`] so the producer end can be stored inside
/// `Inner` without a bootstrap cycle.
pub(super) fn spawn_worker(weak: Weak<Inner>, mut rx: mpsc::Receiver<VoiceTxItem>) {
    tokio::spawn(async move {
        let mut last_send: Option<Instant> = None;
        while let Some(item) = rx.recv().await {
            // Sleep just enough to honour the configured pacing.
            let mut waited = Duration::ZERO;
            if let Some(prev) = last_send {
                let elapsed = prev.elapsed();
                if elapsed < item.pacing {
                    waited = item.pacing - elapsed;
                    tokio::time::sleep(waited).await;
                }
            }
            // Re-anchor a strong handle every iteration so the worker
            // exits as soon as the last external `MeshtasticService` is gone.
            let Some(inner) = weak.upgrade() else { break };
            // Firmware-driven backpressure. The radio publishes a
            // `QueueStatus { free, maxlen }` after every accept/drain.
            // If `free` drops to/below `RADIO_QUEUE_LOW_WATER` we wait
            // for the next update before pushing another frame; this
            // is what keeps a long voice burst from overflowing the
            // firmware's outbound queue and rebooting the sender.
            //
            // `free` defaults to u32::MAX before the first report so
            // the very first send isn't gated. The notify-wait is
            // bounded by `RADIO_QUEUE_WAIT_TIMEOUT` so a missed update
            // can't stall the worker forever.
            let mut bp_waited = Duration::ZERO;
            loop {
                let free = *inner.radio_queue_free.lock();
                if free > RADIO_QUEUE_LOW_WATER {
                    break;
                }
                let bp_start = Instant::now();
                // Cap the wait so a missed/late QueueStatus can't stall
                // the worker forever — re-check after the timeout and
                // retry the send if the firmware still hasn't reported.
                //
                // Race note: the inbound handler uses `notify_one()`,
                // which stores a permit if no waiter is currently
                // registered. So a `QueueStatus` arriving between the
                // `free` read above and the `.notified()` registration
                // below cannot be lost — the next `.notified().await`
                // consumes the stored permit and returns immediately,
                // and we re-check `free` to confirm. (Earlier code used
                // `notify_waiters()`, which only wakes already-registered
                // waiters and silently dropped pre-arrival notifies,
                // causing the worker to burn the full
                // `RADIO_QUEUE_WAIT_TIMEOUT` per frame and the firmware
                // queue to drain dry mid-burst.)
                let waited_for = tokio::time::timeout(
                    RADIO_QUEUE_WAIT_TIMEOUT,
                    inner.radio_queue_notify.notified(),
                )
                .await;
                bp_waited += bp_start.elapsed();
                if waited_for.is_err() {
                    // Timed out waiting; assume the QueueStatus pipeline
                    // is quiet (no traffic yet) and proceed.
                    break;
                }
            }
            let svc = MeshtasticService { inner };
            let frame_len = item.frame.len();
            let send_start = Instant::now();
            let res = svc
                .send_data(
                    PRIVATE_APP as i32,
                    item.frame,
                    item.channel,
                    item.to,
                    item.want_ack,
                    false, // want_response — voice frames are one-shot, no reply expected
                )
                .await;
            let send_elapsed = send_start.elapsed();
            last_send = Some(Instant::now());
            let queue_depth = rx.len();
            match &res {
                Ok(id) => debug!(
                    packet_id = id,
                    bytes = frame_len,
                    paced_ms = waited.as_millis() as u64,
                    bp_ms = bp_waited.as_millis() as u64,
                    send_ms = send_elapsed.as_millis() as u64,
                    queue_depth,
                    "voice tx queue: sent"
                ),
                Err(e) => warn!(
                    ?e,
                    bytes = frame_len,
                    paced_ms = waited.as_millis() as u64,
                    bp_ms = bp_waited.as_millis() as u64,
                    send_ms = send_elapsed.as_millis() as u64,
                    queue_depth,
                    "voice tx queue: send failed"
                ),
            }
            if let Some(d) = item.done {
                let _ = d.send(res);
            }
        }
    });
}

impl MeshtasticService {
    /// Enqueue a single voice frame for paced transmission. Returns once
    /// the item is in the queue; the actual radio send happens later on
    /// the worker task. Use [`Self::enqueue_voice_frame_with_id`] if you
    /// need the assigned packet id.
    pub async fn enqueue_voice_frame(
        &self,
        frame: Vec<u8>,
        channel: u32,
        to: Option<u32>,
        want_ack: bool,
        pacing: Duration,
    ) -> Result<()> {
        self.inner
            .voice_tx
            .send(VoiceTxItem {
                frame,
                channel,
                to,
                want_ack,
                pacing,
                done: None,
            })
            .await
            .map_err(|_| Error::NotConnected)
    }

    /// Like [`Self::enqueue_voice_frame`] but waits for the worker to
    /// actually transmit the frame and returns the packet id. Mostly
    /// useful for `send_voice` which collects ids for the caller.
    pub async fn enqueue_voice_frame_with_id(
        &self,
        frame: Vec<u8>,
        channel: u32,
        to: Option<u32>,
        want_ack: bool,
        pacing: Duration,
    ) -> Result<u32> {
        let (done_tx, done_rx) = oneshot::channel();
        self.inner
            .voice_tx
            .send(VoiceTxItem {
                frame,
                channel,
                to,
                want_ack,
                pacing,
                done: Some(done_tx),
            })
            .await
            .map_err(|_| Error::NotConnected)?;
        done_rx.await.map_err(|_| Error::NotConnected)?
    }
}
