//! High-level outbound voice pipeline.
//!
//! Frontends (CLI, GUI, Android) used to each carry their own copy of
//! the "chunk audio → send burst → linger for NACKs → retransmit"
//! state machine. That code drifted between implementations and forced
//! every new frontend to re-learn the wire protocol.
//!
//! [`VoiceSender`] collapses that loop into a single core component.
//! Callers hand it an audio buffer plus a [`SendRequest`], and it:
//!
//! 1. Builds the wire frames via [`build_message`].
//! 2. Registers them with the shared [`OutgoingVoiceRegistry`] so NACKs
//!    can be serviced.
//! 3. Spawns a background task that paces the initial burst through
//!    [`MeshtasticService::enqueue_voice_frame_with_id`].
//! 4. Listens on `subscribe_data()` for inbound NACKs targeting any of
//!    its in-flight messages; dispatches retransmits through the same
//!    paced, QueueStatus-gated path as the original burst.
//! 5. Emits a stream of [`SendStatus`] events the frontend can render.
//!
//! The "shared model" means **one** [`VoiceSender`] per [`MeshtasticService`]
//! handles every concurrent send. A single NACK-listener task watches
//! the data broadcast and dispatches by `message_id`, instead of one
//! task per send fighting over the same channel.
//!
//! ## Frontend surface
//!
//! ```ignore
//! let handle = svc.send_voice_audio(SendRequest {
//!     audio,
//!     codec: VoiceCodec::AmrNb,
//!     codec_param: 5,
//!     channel: 0,
//!     to: Some(node_num),
//!     parity_count: 4,
//!     ..Default::default()
//! }).await?;
//!
//! let mut rx = handle.subscribe();
//! while let Ok(status) = rx.recv().await {
//!     match status {
//!         SendStatus::Sending { sent, total } => println!("{sent}/{total}"),
//!         SendStatus::Complete { .. } | SendStatus::GaveUp { .. }
//!             | SendStatus::Failed { .. } => break,
//!         _ => {}
//!     }
//! }
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::runtime::Handle;
use tokio::sync::{Semaphore, broadcast};
use tracing::{debug, info, warn};
use web_time::Instant;

use crate::meshtastic::MeshtasticService;
use crate::ports::PRIVATE_APP;
use crate::voice::builder::{BuildConfig, build_message, random_message_id};
use crate::voice::consts::MAX_BODY_SIZE;
use crate::voice::error::VoiceError;
use crate::voice::header::ChunkHeader;
use crate::voice::nack::parse_nack_body;
use crate::voice::outgoing::{DeferOutcome, OutgoingVoiceRegistry, RetransmitSkipReason};
use crate::voice::types::{ModemPreset, PacketType, VoiceCodec};

/// Default linger window after the initial burst. Kept in sync with
/// `DEFAULT_RETAIN_TTL` (the outgoing registry's retain TTL) and the
/// receiver-side `AssemblerConfig::message_timeout` default so the
/// sender stays alive to service NACK rounds for as long as the receiver
/// is willing to try, and the registry keeps the chunks available for the
/// full duration. The previous value of 600 s was too short: on LongFast
/// (900 ms pacing) a 155-frame burst alone takes ~140 s, and subsequent
/// NACK recovery rounds with multi-second cooldowns can easily consume
/// another 600 s before all missing chunks arrive.
pub const DEFAULT_LINGER: Duration = crate::voice::outgoing::DEFAULT_RETAIN_TTL;

/// Upper bound on how long `run_send` waits, after the linger window, for
/// any still-scheduled retransmit batches to drain before emitting
/// `Complete`. Generous enough to cover a cooldown-deferred wake-up plus its
/// paced batch, but bounded so a wedged TX worker can't pin the send entry
/// forever.
const RETRANSMIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(45);

/// Inputs to [`VoiceSender::send`]. All defaults are sensible; the only
/// always-required fields are [`Self::audio`] and [`Self::codec`].
#[derive(Debug, Clone)]
pub struct SendRequest {
    /// Raw codec frame bytes — no container header. The protocol
    /// carries opaque codec bytes; callers responsible for any
    /// container stripping (e.g. AMR `#!AMR\n`) before passing in.
    pub audio: Vec<u8>,
    pub codec: VoiceCodec,
    pub codec_param: u8,
    /// Meshtastic channel index. `0` for the primary channel.
    pub channel: u32,
    /// Destination node number, or `None` for a channel broadcast.
    pub to: Option<u32>,
    /// Reed-Solomon parity shards; `0` disables FEC (NACK still works).
    pub parity_count: u8,
    /// Override the per-message chunk size. `None` means use
    /// [`MAX_BODY_SIZE`] (best throughput for short-range presets).
    pub chunk_size: Option<usize>,
    /// How long to keep the registry entry alive after the initial
    /// burst so late NACK rounds can be serviced. `None` ⇒
    /// [`DEFAULT_LINGER`].
    pub linger: Option<Duration>,
    /// Per-(from, channel) monotonic stream sequence. The protocol
    /// treats it as informational; default `0` is fine for one-shot
    /// recordings.
    pub stream_seq: u8,
    /// Marks the final frame of a recording session. Receivers MAY
    /// use this to expire stream-history state.
    pub last_in_stream: bool,
    /// Optional override for the inter-frame TX pacing. `None` lets
    /// the sender read the current modem preset off
    /// [`MeshtasticService::watch_lora_config`]; if that snapshot isn't
    /// available yet we fall back to [`ModemPreset::fallback_pacing`].
    pub pacing: Option<Duration>,
}

impl Default for SendRequest {
    fn default() -> Self {
        Self {
            audio: Vec::new(),
            codec: VoiceCodec::AmrNb,
            codec_param: 0,
            channel: 0,
            to: None,
            parity_count: 0,
            chunk_size: None,
            linger: None,
            stream_seq: 0,
            last_in_stream: true,
            pacing: None,
        }
    }
}

/// Lifecycle event emitted on the [`SendHandle::subscribe`] channel.
///
/// The stream is guaranteed to terminate with exactly one of
/// [`SendStatus::Complete`], [`SendStatus::GaveUp`], or
/// [`SendStatus::Failed`]; subscribers can break on any of those.
#[derive(Debug, Clone)]
pub enum SendStatus {
    /// Wire frames have been built. Carries the structure of the
    /// upcoming burst so a UI can render a progress bar with the
    /// right scale.
    Building {
        message_id: u32,
        total_data: u8,
        parity_count: u8,
    },
    /// One more frame from the initial burst has been handed to the
    /// voice TX worker. `sent` includes both DATA and PARITY frames.
    Sending {
        message_id: u32,
        sent: u32,
        total: u32,
    },
    /// All frames of the initial burst are now on the wire (or in the
    /// firmware queue). The sender remains alive for the linger window
    /// to service late NACK rounds.
    BurstComplete {
        message_id: u32,
        packet_ids: Vec<u32>,
    },
    /// A NACK round triggered a retransmit. `chunks` lists the data
    /// chunk indices actually re-enqueued (after pending-chunk dedup
    /// and per-message cooldown). May be smaller than the receiver's
    /// bitmap if some chunks were already in flight.
    Retransmitting { message_id: u32, chunks: Vec<u8> },
    /// Linger window elapsed without further NACKs. The receiver may or
    /// may not have completed reassembly — this status is the sender's
    /// best-effort signal that it's safe to drop UI state.
    Complete { message_id: u32 },
    /// Receiver sent a NACK with `give_up = true`. The sender dropped
    /// the registry entry and will not retransmit further.
    GaveUp { message_id: u32 },
    /// Terminal error — either the initial burst failed to enqueue or
    /// the build step itself errored.
    Failed { message_id: u32, message: String },
}

impl SendStatus {
    /// Returns `true` if this status terminates the stream — i.e. the
    /// caller may stop reading.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Complete { .. } | Self::GaveUp { .. } | Self::Failed { .. }
        )
    }

    /// The `message_id` this status belongs to. Always available; useful
    /// when a single subscriber multiplexes several sends.
    pub fn message_id(&self) -> u32 {
        match self {
            Self::Building { message_id, .. }
            | Self::Sending { message_id, .. }
            | Self::BurstComplete { message_id, .. }
            | Self::Retransmitting { message_id, .. }
            | Self::Complete { message_id }
            | Self::GaveUp { message_id }
            | Self::Failed { message_id, .. } => *message_id,
        }
    }
}

/// Handle returned by [`VoiceSender::send`]. Owns the broadcast
/// receiver for status events; cheap to clone via the [`Self::subscribe`]
/// method (each call yields a fresh receiver).
#[derive(Debug, Clone)]
pub struct SendHandle {
    pub message_id: u32,
    status_tx: broadcast::Sender<SendStatus>,
}

impl SendHandle {
    /// Subscribe to lifecycle events. The receiver buffer is bounded;
    /// slow consumers will see `RecvError::Lagged`. Buffer size is
    /// generous (256) since events are small and bursts are short.
    pub fn subscribe(&self) -> broadcast::Receiver<SendStatus> {
        self.status_tx.subscribe()
    }
}

/// Per-message in-flight state held by the [`VoiceSender`]. One entry is
/// created when [`VoiceSender::send`] returns and removed when the send
/// terminates (Complete / GaveUp / Failed).
struct ActiveSend {
    status_tx: broadcast::Sender<SendStatus>,
    channel: u32,
    to: Option<u32>,
}

/// Shared outbound voice pipeline. One instance per [`MeshtasticService`].
///
/// Internally `Arc`'d — the background NACK-listener task holds a
/// `Weak` reference and shuts down once the last external clone of
/// the sender drops.
pub struct VoiceSender {
    svc: MeshtasticService,
    registry: Arc<OutgoingVoiceRegistry>,
    active: Mutex<HashMap<u32, ActiveSend>>,
    /// Tokio runtime handle captured at construction. All background
    /// tasks (NACK listener, per-send burst, retransmit dispatch) are
    /// spawned through this so callers (UI threads, foreign FFI
    /// callbacks) don't need an entered runtime context.
    rt: Handle,
    /// Limits concurrent retransmit tasks spawned from the NACK listener
    /// to prevent unbounded task growth when many messages are NACK'd
    /// simultaneously.
    retransmit_permits: Arc<Semaphore>,
    /// Diagnostic counter: NACKs dropped due to listener lagging behind
    /// the broadcast channel. High values indicate the NACK listener task
    /// cannot keep up with the message arrival rate.
    lagged_nack_count: AtomicU64,
    /// In-flight retransmit task count per message. A retransmit is counted
    /// from the moment it is scheduled (immediate dispatch, or a deferred
    /// wake-up that is still sleeping) until its batch finishes draining
    /// through the paced worker. `run_send` waits for this to hit zero before
    /// emitting `Complete`, so subscribers never see the terminal status
    /// while a retransmit is still enqueueing frames.
    retransmit_inflight: Mutex<HashMap<u32, u32>>,
    /// Pulsed whenever a retransmit task finishes, so a `run_send` waiting to
    /// finalize can re-check the in-flight count promptly.
    retransmit_drained: Arc<tokio::sync::Notify>,
}

impl VoiceSender {
    /// Construct a new sender bound to `svc`. Spawns the background
    /// NACK-listener task; the task lifetime is tied to the returned
    /// `Arc` via `Arc::downgrade`.
    ///
    /// Must be called from within a tokio runtime context (so the
    /// runtime [`Handle`] can be captured). Frontends running off the
    /// runtime thread (egui UI, JNI callbacks) should wrap the call in
    /// `rt.enter()` or use [`Self::new_on`].
    pub fn new(svc: MeshtasticService) -> Arc<Self> {
        Self::new_on(svc, Handle::current())
    }

    /// Like [`Self::new`] but takes an explicit runtime [`Handle`].
    /// Use this when no runtime is entered on the calling thread.
    pub fn new_on(svc: MeshtasticService, rt: Handle) -> Arc<Self> {
        let sender = Arc::new(Self {
            svc: svc.clone(),
            registry: Arc::new(OutgoingVoiceRegistry::default()),
            active: Mutex::new(HashMap::new()),
            rt: rt.clone(),
            retransmit_permits: Arc::new(Semaphore::new(16)),
            lagged_nack_count: AtomicU64::new(0),
            retransmit_inflight: Mutex::new(HashMap::new()),
            retransmit_drained: Arc::new(tokio::sync::Notify::new()),
        });
        let weak = Arc::downgrade(&sender);
        let rx = svc.subscribe_data();
        rt.spawn(nack_listener_task(weak, rx));
        sender
    }

    /// Build and ship `req`. Returns a handle whose
    /// [`SendHandle::subscribe`] yields a stream of [`SendStatus`]
    /// events; the stream terminates with exactly one of `Complete`,
    /// `GaveUp`, or `Failed`.
    ///
    /// Returns early with `Err` only for synchronous build-time errors
    /// (empty audio, oversized message). Runtime errors during the
    /// background burst surface as `SendStatus::Failed`.
    pub fn send(self: &Arc<Self>, req: SendRequest) -> Result<SendHandle, VoiceError> {
        let message_id = self.fresh_message_id()?;

        // Clamp the per-chunk body size to whatever the underlying
        // transport can deliver intact in a single write. For BLE this
        // is `negotiated_MTU − 3 − ToRadio_overhead − HEADER_SIZE`;
        // for USB serial / loopback it's `MAX_BODY_SIZE`. Honour an
        // explicit override from the caller but still cap it at the
        // transport limit — a caller asking for a body bigger than the
        // wire can carry would otherwise silently lose chunks past
        // the transport's truncation point.
        let transport_max_body = self.svc.max_voice_body_size().min(MAX_BODY_SIZE);
        let chunk_size = req
            .chunk_size
            .unwrap_or(transport_max_body)
            .min(transport_max_body);
        let cfg = BuildConfig {
            message_id,
            stream_seq: req.stream_seq,
            codec: req.codec,
            codec_param: req.codec_param,
            chunk_size,
            parity_count: req.parity_count,
            last_in_stream: req.last_in_stream,
        };
        let encoded = build_message(&req.audio, &cfg)?;
        let total_frames = encoded.frames.len() as u32;
        let total_data = encoded.total_data;
        let parity_count = encoded.parity_count;

        // Broadcast buffer is generous: a long Long-Slow burst can emit
        // hundreds of `Sending` events back-to-back, and a momentarily
        // suspended subscriber shouldn't see `Lagged`.
        let (status_tx, _) = broadcast::channel(256);
        let _ = status_tx.send(SendStatus::Building {
            message_id,
            total_data,
            parity_count,
        });

        self.registry
            .register(message_id, &encoded, req.channel, req.to);
        self.active.lock().insert(
            message_id,
            ActiveSend {
                status_tx: status_tx.clone(),
                channel: req.channel,
                to: req.to,
            },
        );

        let pacing = req.pacing.unwrap_or_else(|| self.current_pacing());
        let linger = req.linger.unwrap_or(DEFAULT_LINGER);
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            this.run_send(
                message_id,
                encoded.frames,
                total_frames,
                req,
                pacing,
                linger,
            )
            .await;
        });

        Ok(SendHandle {
            message_id,
            status_tx,
        })
    }

    /// Allocate a `message_id` that doesn't alias an in-flight send. A
    /// random non-zero u32 collision is astronomically unlikely, but a single
    /// one would let two concurrent sends share a registry/active slot (the
    /// first send would then operate on the second's frames), so re-roll on
    /// the off chance. Bounded retries: after a few attempts we accept the
    /// last roll rather than loop, since the id space is 2^32.
    fn fresh_message_id(&self) -> Result<u32, VoiceError> {
        for _ in 0..8 {
            let id = random_message_id()?;
            // Evaluate (and release) the active lock before touching the
            // registry lock so the two are never held nested.
            let in_active = self.active.lock().contains_key(&id);
            if !in_active && !self.registry.contains(id) {
                return Ok(id);
            }
        }
        random_message_id()
    }

    /// Read the LoRa modem preset off the service and convert it to
    /// pacing. Falls back to [`ModemPreset::fallback_pacing`] if the
    /// preset hasn't been observed yet (e.g. just-connected radio).
    fn current_pacing(&self) -> Duration {
        let preset = self
            .svc
            .watch_lora_config()
            .borrow()
            .as_ref()
            .and_then(|l| crate::meshtastic::service::modem_preset_from_proto(l.modem_preset));
        preset
            .map(ModemPreset::pacing)
            .unwrap_or_else(ModemPreset::fallback_pacing)
    }

    /// Background task: pace the initial burst, then linger for NACKs.
    async fn run_send(
        self: Arc<Self>,
        message_id: u32,
        frames: Vec<Vec<u8>>,
        total_frames: u32,
        req: SendRequest,
        pacing: Duration,
        linger: Duration,
    ) {
        let SendRequest { channel, to, .. } = req;
        let want_ack = to.is_some();
        let mut packet_ids = Vec::with_capacity(frames.len());
        let active_status = self
            .active
            .lock()
            .get(&message_id)
            .map(|a| a.status_tx.clone());
        let Some(status_tx) = active_status else {
            // Race: someone removed our entry before we even started.
            // Nothing useful to do.
            return;
        };

        // `total_data` bounds the data-chunk slice; parity frames live
        // past it and don't have a NACK-addressable index.
        let total_data = self
            .registry
            .data_count(message_id)
            .unwrap_or(total_frames as u8);
        for (i, frame) in frames.into_iter().enumerate() {
            // A give-up NACK arriving mid-burst removes the active entry and
            // emits `GaveUp`. Stop enqueueing the rest of the burst rather
            // than burning airtime the receiver explicitly asked us to stop
            // (on slow presets the burst can run for minutes). The give-up
            // handler already emitted the terminal status, so we just return.
            if !self.active.lock().contains_key(&message_id) {
                debug!(message_id, "voice: send cancelled mid-burst (give-up NACK)");
                return;
            }
            match self
                .svc
                .enqueue_voice_frame_with_id(frame, channel, to, want_ack, pacing)
                .await
            {
                Ok(id) => {
                    packet_ids.push(id);
                    // Release the per-data-chunk pending flag seeded by
                    // `register()` so a NACK arriving mid-burst can
                    // start servicing chunks that have actually left
                    // the radio. Parity frames don't participate.
                    if i < total_data as usize {
                        self.registry.mark_chunk_sent(message_id, i as u8);
                    }
                    let _ = status_tx.send(SendStatus::Sending {
                        message_id,
                        sent: (i + 1) as u32,
                        total: total_frames,
                    });
                }
                Err(e) => {
                    warn!(message_id, ?e, "voice initial burst enqueue failed");
                    let _ = status_tx.send(SendStatus::Failed {
                        message_id,
                        message: format!("burst enqueue failed: {e}"),
                    });
                    self.cleanup(message_id);
                    return;
                }
            }
        }
        let _ = status_tx.send(SendStatus::BurstComplete {
            message_id,
            packet_ids,
        });

        // Linger window: stay registered so late NACK rounds can be
        // serviced. The NACK listener task does the actual retransmits;
        // we just keep the entry alive for the timer.
        tokio::time::sleep(linger).await;

        // Don't finalize while a retransmit batch is still scheduled or
        // draining: subscribers may tear down on `Complete`, and emitting it
        // mid-retransmit would violate the documented terminal-status
        // ordering. We stay registered (so new NACKs keep extending the
        // count) until the in-flight retransmits drain, bounded so a stuck
        // worker can't pin the entry indefinitely.
        let drain_deadline = Instant::now() + RETRANSMIT_DRAIN_TIMEOUT;
        while self.inflight_count(message_id) > 0 {
            let remaining = drain_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                warn!(
                    message_id,
                    "retransmits still in flight at drain timeout; finalizing anyway"
                );
                break;
            }
            // Cap the wait per iteration so a notification delivered before we
            // started awaiting can't make us miss the drain.
            let _ = tokio::time::timeout(
                remaining.min(Duration::from_millis(100)),
                self.retransmit_drained.notified(),
            )
            .await;
        }

        // Atomically claim the terminal status: whoever removes the active
        // entry owns it. A give_up NACK handler running concurrently does the
        // same `active.remove`, so exactly one of us emits a terminal status —
        // this closes the check-then-act race that could emit both `GaveUp`
        // and `Complete`. `registry.remove` is idempotent, so it is safe to
        // call regardless of who claimed.
        let claimed = self.active.lock().remove(&message_id).is_some();
        if claimed {
            let _ = status_tx.send(SendStatus::Complete { message_id });
        }
        self.registry.remove(message_id);
        // Prune expired outgoing entries to keep memory usage low.
        self.registry.prune_expired();
    }

    /// Drop the per-message state on terminal status.
    fn cleanup(&self, message_id: u32) {
        self.active.lock().remove(&message_id);
        self.registry.remove(message_id);
    }

    /// Record that a retransmit task for `message_id` has been scheduled.
    /// Paired with exactly one [`Self::inflight_dec`] via [`InflightGuard`].
    fn inflight_inc(&self, message_id: u32) {
        *self
            .retransmit_inflight
            .lock()
            .entry(message_id)
            .or_insert(0) += 1;
    }

    /// Record that a retransmit task for `message_id` has finished.
    fn inflight_dec(&self, message_id: u32) {
        {
            let mut map = self.retransmit_inflight.lock();
            if let Some(c) = map.get_mut(&message_id) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    map.remove(&message_id);
                }
            }
        }
        // Wake any run_send waiting to finalize this message.
        self.retransmit_drained.notify_waiters();
    }

    /// Number of retransmit tasks currently scheduled or in flight for
    /// `message_id`.
    fn inflight_count(&self, message_id: u32) -> u32 {
        self.retransmit_inflight
            .lock()
            .get(&message_id)
            .copied()
            .unwrap_or(0)
    }

    /// Diagnostic: number of in-flight sends.
    pub fn len(&self) -> usize {
        self.active.lock().len()
    }

    /// Returns `true` if no sends are currently in flight.
    pub fn is_empty(&self) -> bool {
        self.active.lock().is_empty()
    }

    /// Diagnostic: number of NACKs dropped due to listener lagging.
    /// High values indicate the NACK listener cannot keep up with the
    /// message arrival rate and retransmit requests are being lost.
    pub fn lagged_nack_count(&self) -> u64 {
        self.lagged_nack_count.load(Ordering::Relaxed)
    }

    /// Tune how long the internal [`OutgoingVoiceRegistry`] retains
    /// frames after a send for late NACK rounds. Should typically match
    /// the receiver's `AssemblerConfig::message_timeout` so a NACK can
    /// never arrive for a frame we've already forgotten.
    pub fn set_retain_ttl(&self, ttl: Duration) {
        self.registry.set_retain_ttl(ttl);
    }
}

/// NACK-listener task. One per [`VoiceSender`] for the lifetime of the
/// sender; subscribes to the service's `data` broadcast, filters for
/// NACK frames addressed to one of our in-flight messages, and
/// dispatches retransmits.
///
/// A NACK is only trusted if it comes from the node we unicast to.
///
/// `to` is the destination of our send (`None` = broadcast). Broadcast
/// sends are never NACKed by a conforming receiver, so any NACK against a
/// broadcast send is dropped; for a unicast send only the destination node
/// may NACK.
fn nack_source_allowed(to: Option<u32>, nack_from: u32) -> bool {
    matches!(to, Some(dest) if dest == nack_from)
}

/// Uses `Weak<VoiceSender>` so that dropping the last external sender
/// clone terminates the task on the next message.
async fn nack_listener_task(
    weak: Weak<VoiceSender>,
    mut rx: broadcast::Receiver<crate::service::IncomingData>,
) {
    loop {
        let data = match rx.recv().await {
            Ok(d) => d,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(
                    skipped = n,
                    "voice sender NACK listener lagged; NACKs may have been dropped"
                );
                // Track lagging for diagnostics: high values indicate listener overload
                if let Some(sender) = weak.upgrade() {
                    sender.lagged_nack_count.fetch_add(n, Ordering::Relaxed);
                }
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        let Some(sender) = weak.upgrade() else { break };
        if data.portnum != PRIVATE_APP as i32 {
            continue;
        }
        // Cheap version + packet-type filter before locking anything.
        // Peek the message_id from the raw bytes, look it up, then
        // re-parse the whole header. Frames for messages we know
        // nothing about are dropped without a full parse.
        let Some(message_id) = ChunkHeader::peek_message_id(&data.payload) else {
            continue;
        };
        let entry = {
            let map = sender.active.lock();
            map.get(&message_id)
                .map(|a| (a.status_tx.clone(), a.channel, a.to))
        };
        let Some((status_tx, channel, to)) = entry else {
            continue;
        };
        let Ok((header, body)) = ChunkHeader::parse(&data.payload) else {
            continue;
        };
        if header.packet_type != PacketType::Nack {
            continue;
        }
        let Ok(nack) = parse_nack_body(&header, body) else {
            continue;
        };

        // Only trust a NACK from the node we are unicasting to. Broadcast
        // sends are never NACKed by a conforming receiver (the assembler
        // suppresses broadcast NACKs), so any NACK against a broadcast send
        // is forged or stale. Without this, any third node could observe an
        // on-air message_id, compute the unkeyed header MAC, and forge a
        // give-up NACK to cancel someone else's send (or drive retransmits).
        if !nack_source_allowed(to, data.from) {
            debug!(
                message_id,
                from = data.from,
                ?to,
                "voice: dropping NACK from untrusted source"
            );
            continue;
        }

        if nack.give_up {
            info!(message_id = nack.message_id, "voice: receiver gave up");
            let _ = status_tx.send(SendStatus::GaveUp {
                message_id: nack.message_id,
            });
            sender.cleanup(nack.message_id);
            continue;
        }

        let pacing = sender.current_pacing();
        // Service the NACK round. The take/defer handoff has a narrow window
        // where the cooldown can lapse *between* take_retransmit reporting it
        // active and defer_nack running; if that happens defer_nack reports
        // `CooldownElapsed` and we retry the take rather than dropping the
        // whole round (which would force the receiver to time out and re-NACK).
        // Bounded so a pathological cooldown flap can't spin the listener.
        for attempt in 0..3 {
            match sender
                .registry
                .take_retransmit(nack.message_id, &nack.missing, pacing)
            {
                Ok(plan) if plan.is_empty() => {
                    debug!(
                        message_id = nack.message_id,
                        requested = nack.missing.len(),
                        "voice: no frames to retransmit (all pending)"
                    );
                    break;
                }
                Ok(plan) => {
                    spawn_plan_dispatch(
                        &sender,
                        plan,
                        nack.message_id,
                        channel,
                        to,
                        pacing,
                        &status_tx,
                    );
                    break;
                }
                Err(RetransmitSkipReason::CooldownActive) => {
                    // Cooldown gates the request: stash the missing list and
                    // schedule a wake-up task to retry once the previous batch
                    // has cleared the radio.
                    match sender
                        .registry
                        .defer_nack(nack.message_id, nack.missing.clone())
                    {
                        DeferOutcome::Scheduled(deadline) => {
                            debug!(
                                message_id = nack.message_id,
                                requested = nack.missing.len(),
                                "voice: retransmit deferred (cooldown active)"
                            );
                            // Count the deferred wake-up as in-flight from now
                            // (it is still sleeping) so a short linger can't
                            // finalize the send before the deferred batch ships.
                            sender.inflight_inc(nack.message_id);
                            spawn_deferred_retransmit(
                                Arc::downgrade(&sender),
                                nack.message_id,
                                channel,
                                to,
                                status_tx.clone(),
                                deadline,
                            );
                            break;
                        }
                        DeferOutcome::AlreadyScheduled => {
                            debug!(
                                message_id = nack.message_id,
                                "voice: retransmit deferred (existing wake-up will service)"
                            );
                            break;
                        }
                        DeferOutcome::CooldownElapsed => {
                            // Cooldown lapsed mid-handoff: retry the take so
                            // the round isn't lost.
                            debug!(
                                message_id = nack.message_id,
                                attempt, "voice: cooldown lapsed mid-handoff, retrying take"
                            );
                            continue;
                        }
                        DeferOutcome::Gone => break,
                    }
                }
                Err(reason) => {
                    // Log skip reason at appropriate level for diagnostics
                    let msg = match reason {
                        RetransmitSkipReason::TtlExpired => "message expired",
                        RetransmitSkipReason::BudgetExhausted => "max retransmits exceeded",
                        RetransmitSkipReason::AllChunksPending => "all chunks already pending",
                        RetransmitSkipReason::CooldownActive => unreachable!("handled above"),
                    };
                    debug!(
                        message_id = nack.message_id,
                        requested = nack.missing.len(),
                        reason = msg,
                        "voice: retransmit skipped"
                    );
                    break;
                }
            }
        }
    }
}

/// Decrements a message's in-flight retransmit count when dropped, so every
/// scheduled retransmit (immediate or deferred) is balanced on every exit
/// path of its task. Holds a `Weak` so a pending task can't keep the sender
/// alive past its normal `Weak`-driven shutdown.
struct InflightGuard {
    sender: Weak<VoiceSender>,
    message_id: u32,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.upgrade() {
            sender.inflight_dec(self.message_id);
        }
    }
}

/// Emit `Retransmitting` and spawn the detached, semaphore-gated task that
/// ships one retransmit `plan` through the paced TX worker. Detached so a slow
/// worker doesn't block the NACK listener from processing the next inbound
/// frame. The send is counted as in-flight until the batch drains so
/// `run_send` won't finalize mid-retransmit. `to` reproduces the original
/// send's addressing (unicast stays unicast, broadcast stays broadcast).
fn spawn_plan_dispatch(
    sender: &Arc<VoiceSender>,
    plan: Vec<(u8, bytes::Bytes)>,
    message_id: u32,
    channel: u32,
    to: Option<u32>,
    pacing: Duration,
    status_tx: &broadcast::Sender<SendStatus>,
) {
    let scheduled: Vec<u8> = plan.iter().map(|(idx, _)| *idx).collect();
    info!(
        message_id,
        scheduled = scheduled.len(),
        "voice: retransmitting"
    );
    let _ = status_tx.send(SendStatus::Retransmitting {
        message_id,
        chunks: scheduled,
    });

    sender.inflight_inc(message_id);
    let guard = InflightGuard {
        sender: Arc::downgrade(sender),
        message_id,
    };
    let permits = Arc::clone(&sender.retransmit_permits);
    let svc = sender.svc.clone();
    let registry = Arc::clone(&sender.registry);
    tokio::spawn(async move {
        let _guard = guard;
        let _permit = match permits.acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                warn!(message_id, "retransmit semaphore closed");
                return;
            }
        };
        dispatch_retransmit_batch(&svc, &registry, plan, message_id, channel, to, pacing).await;
    });
}

/// Push one retransmit batch through the paced TX worker, clearing
/// `pending_chunks` per-frame so later NACK rounds can request any
/// chunks the radio fails to enqueue.
async fn dispatch_retransmit_batch(
    svc: &MeshtasticService,
    registry: &OutgoingVoiceRegistry,
    plan: Vec<(u8, bytes::Bytes)>,
    message_id: u32,
    channel: u32,
    to: Option<u32>,
    pacing: Duration,
) {
    let want_ack = to.is_some();
    // Pre-extract indices so the failure path can release `pending_chunks`
    // for the un-sent tail after `plan` has been consumed by move.
    let indices: Vec<u8> = plan.iter().map(|(i, _)| *i).collect();
    for (batch_idx, (idx, frame)) in plan.into_iter().enumerate() {
        // `enqueue_voice_frame_with_id` (and ultimately prost) needs an
        // owned `Vec<u8>`. The deep copy was previously paid inside the
        // registry lock by `frames_for`; moving it here keeps the lock
        // hold time at O(N atomic increments) instead of O(N memcpy).
        let r = svc
            .enqueue_voice_frame_with_id(frame.to_vec(), channel, to, want_ack, pacing)
            .await;
        if let Err(e) = r {
            warn!(message_id, idx, ?e, "voice retransmit enqueue failed");
            // Mark the failed chunk as sent so it can be retried on the next NACK.
            // Without this, failed chunks stay stuck in `pending_chunks` forever.
            registry.mark_chunk_sent(message_id, idx);
            // Also clear pending for the un-sent tail of this batch so
            // a subsequent NACK round can retry them.
            for tail_idx in &indices[(batch_idx + 1)..] {
                registry.mark_chunk_sent(message_id, *tail_idx);
            }
            return;
        }
        // P0: Only mark sent after successful enqueue (was bug: marked before check)
        registry.mark_chunk_sent(message_id, idx);
    }
}

/// Schedule a one-shot task that fires at `deadline` (the message's
/// `cooldown_until`) and processes the deferred missing list stashed by
/// [`OutgoingVoiceRegistry::defer_nack`]. Idempotent at the registry
/// level: a second `defer_nack` during the same cooldown updates the
/// stashed list but does not spawn a duplicate task.
fn spawn_deferred_retransmit(
    weak: Weak<VoiceSender>,
    message_id: u32,
    channel: u32,
    to: Option<u32>,
    status_tx: broadcast::Sender<SendStatus>,
    deadline: Instant,
) {
    tokio::spawn(async move {
        // Sleep just past the deadline so `take_retransmit` sees
        // cooldown as elapsed. A small grace absorbs scheduler jitter
        // and any clock skew between `Instant::now()` calls.
        let now = Instant::now();
        let wait = deadline
            .checked_duration_since(now)
            .unwrap_or_default()
            .saturating_add(Duration::from_millis(10));
        tokio::time::sleep(wait).await;

        let Some(sender) = weak.upgrade() else { return };
        // Balance the inflight_inc the scheduler did before spawning us, on
        // every exit path from here on (so run_send's drain wait is correct).
        let _guard = InflightGuard {
            sender: weak.clone(),
            message_id,
        };
        let Some(missing) = sender.registry.take_deferred(message_id) else {
            return;
        };

        let pacing = sender.current_pacing();
        let plan = match sender
            .registry
            .take_retransmit(message_id, &missing, pacing)
        {
            Ok(p) if !p.is_empty() => p,
            Ok(_) => return,
            Err(RetransmitSkipReason::CooldownActive) => {
                // A newer NACK round set a fresh cooldown after we woke and
                // already consumed the deferred list via `take_deferred`.
                // Re-stash it and reschedule rather than dropping it, which
                // would defeat the deferral in exactly the contended case it
                // exists for.
                // Scheduled: re-armed our own wake-up. AlreadyScheduled:
                // another task will service it. CooldownElapsed: lapsed again,
                // the receiver will re-NACK. Gone: entry GC'd.
                if let DeferOutcome::Scheduled(new_deadline) =
                    sender.registry.defer_nack(message_id, missing)
                {
                    sender.inflight_inc(message_id);
                    spawn_deferred_retransmit(
                        weak.clone(),
                        message_id,
                        channel,
                        to,
                        status_tx,
                        new_deadline,
                    );
                }
                return;
            }
            Err(reason) => {
                debug!(
                    message_id,
                    requested = missing.len(),
                    reason = ?reason,
                    "voice: deferred retransmit skipped after wake-up"
                );
                return;
            }
        };

        let scheduled: Vec<u8> = plan.iter().map(|(idx, _)| *idx).collect();
        info!(
            message_id,
            requested = missing.len(),
            scheduled = scheduled.len(),
            "voice: retransmitting (deferred)"
        );
        let _ = status_tx.send(SendStatus::Retransmitting {
            message_id,
            chunks: scheduled,
        });

        let _permit = match sender.retransmit_permits.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                warn!(message_id, "retransmit semaphore closed");
                return;
            }
        };
        dispatch_retransmit_batch(
            &sender.svc,
            &sender.registry,
            plan,
            message_id,
            channel,
            to,
            pacing,
        )
        .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_status_terminality() {
        assert!(SendStatus::Complete { message_id: 1 }.is_terminal());
        assert!(SendStatus::GaveUp { message_id: 1 }.is_terminal());
        assert!(
            SendStatus::Failed {
                message_id: 1,
                message: "x".into()
            }
            .is_terminal()
        );
        assert!(
            !SendStatus::Sending {
                message_id: 1,
                sent: 1,
                total: 10
            }
            .is_terminal()
        );
        assert!(
            !SendStatus::Building {
                message_id: 1,
                total_data: 1,
                parity_count: 0
            }
            .is_terminal()
        );
    }

    #[test]
    fn nack_source_allowed_unicast_only_from_dest() {
        assert!(nack_source_allowed(Some(7), 7));
        assert!(!nack_source_allowed(Some(7), 8));
    }

    #[test]
    fn nack_source_allowed_broadcast_rejects_all() {
        assert!(!nack_source_allowed(None, 7));
        assert!(!nack_source_allowed(None, 0));
    }

    /// DEFAULT_LINGER, DEFAULT_RETAIN_TTL, and the AssemblerConfig
    /// message_timeout default must all be equal so the sender stays alive
    /// to service every NACK round the receiver will attempt, and the
    /// outgoing registry keeps chunks available for the whole window.
    #[test]
    fn linger_retain_ttl_and_timeout_are_aligned() {
        use crate::voice::assembler::AssemblerConfig;
        use crate::voice::outgoing::DEFAULT_RETAIN_TTL;
        assert_eq!(
            DEFAULT_LINGER,
            DEFAULT_RETAIN_TTL,
            "sender linger must equal outgoing registry retain TTL"
        );
        assert_eq!(
            DEFAULT_LINGER,
            AssemblerConfig::default().message_timeout,
            "sender linger must equal assembler default message_timeout"
        );
    }

    #[test]
    fn send_request_defaults_are_sensible() {
        let req = SendRequest::default();
        assert!(req.audio.is_empty());
        assert_eq!(req.codec, VoiceCodec::AmrNb);
        assert_eq!(req.channel, 0);
        assert!(req.to.is_none());
        assert_eq!(req.parity_count, 0);
        assert!(req.last_in_stream);
        assert!(req.linger.is_none());
        assert!(req.pacing.is_none());
    }
}
