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
//!    [`MeshService::enqueue_voice_frame_with_id`].
//! 4. Listens on `subscribe_data()` for inbound NACKs targeting any of
//!    its in-flight messages; dispatches retransmits through the same
//!    paced, QueueStatus-gated path as the original burst.
//! 5. Emits a stream of [`SendStatus`] events the frontend can render.
//!
//! The "shared model" means **one** [`VoiceSender`] per [`MeshService`]
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
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::runtime::Handle;
use tokio::sync::{Semaphore, broadcast};
use tracing::{debug, info, warn};

use crate::ports::PRIVATE_APP;
use crate::service::MeshService;
use crate::voice::builder::{BuildConfig, build_message, random_message_id};
use crate::voice::consts::MAX_BODY_SIZE;
use crate::voice::crypto::{EnvelopeKey, derive_key};
use crate::voice::error::VoiceError;
use crate::voice::header::ChunkHeader;
use crate::voice::nack::parse_nack_body;
use crate::voice::outgoing::OutgoingVoiceRegistry;
use crate::voice::types::{ModemPreset, PacketType, VoiceCodec};

/// Default linger window after the initial burst. Matches the
/// receiver-side `AssemblerConfig::message_timeout` default (10 min)
/// so the sender stays alive to service NACK rounds for as long as the
/// receiver is willing to try. The previous value of 60 s was too short
/// for slow modem presets: on LongFast (900 ms pacing) a 155-frame burst
/// alone takes ~140 s, leaving only a 60 s window for potentially dozens
/// of NACK rounds, each subject to a multi-second retransmit cooldown.
pub const DEFAULT_LINGER: Duration = Duration::from_secs(600);

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
    /// AES-GCM envelope key. `None` keeps bodies plaintext (still
    /// inside Meshtastic's channel encryption).
    pub encryption: Option<EnvelopeKey>,
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
    /// [`MeshService::watch_lora_config`]; if that snapshot isn't
    /// available yet we fall back to [`ModemPreset::fallback_pacing`].
    pub pacing: Option<Duration>,
    /// Channel PSK for the trailing 4-byte header MAC. `Some(psk)` ⇒
    /// HMAC-SHA256 (authenticity); `None` ⇒ unkeyed SHA-256
    /// (integrity only). Should match the receiver's
    /// [`crate::voice::AssemblerConfig::channel_psk`].
    pub mac_key: Option<Vec<u8>>,
    /// When set alongside [`mac_key`] the envelope encryption key is
    /// derived *after* the per-message `message_id` has been assigned.
    /// This is the correct path for callers (e.g. Android bridge) that
    /// do not know the `message_id` before calling [`Self::send`].
    ///
    /// If [`Self::encryption`] is `Some` it takes precedence and this
    /// field is ignored.
    pub channel_psk: Option<Vec<u8>>,
    /// Sender's node number, used alongside [`channel_psk`] for
    /// per-message envelope key derivation. Ignored when
    /// [`Self::encryption`] is set.
    pub from_node_num: u32,
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
            encryption: None,
            linger: None,
            stream_seq: 0,
            last_in_stream: true,
            pacing: None,
            mac_key: None,
            channel_psk: None,
            from_node_num: 0,
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
    /// Channel PSK captured from the originating `SendRequest`. The
    /// NACK listener uses this to verify keyed-MAC NACK frames for
    /// this specific message; it is per-active-send rather than
    /// per-sender because callers can in principle target multiple
    /// channels (each with its own PSK) from one [`VoiceSender`].
    mac_key: Option<Vec<u8>>,
    /// Count of currently-executing retransmit tasks for this message.
    /// Capped at 2 to prevent unbounded task spawning during NACK storms.
    pending_retransmit_tasks: u8,
    /// Missing chunk indices from the last NACK for thrashing detection.
    /// If the same chunks are NACKed repeatedly, the receiver is stuck.
    last_missing_set: Vec<u8>,
    /// Count of consecutive NACKs requesting identical missing chunks.
    /// If this reaches 3, we give up on the message as unrecoverable.
    identical_nack_count: u8,
}

/// Shared outbound voice pipeline. One instance per [`MeshService`].
///
/// Internally `Arc`'d — the background NACK-listener task holds a
/// `Weak` reference and shuts down once the last external clone of
/// the sender drops.
pub struct VoiceSender {
    svc: MeshService,
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
    retransmit_permits: Arc<tokio::sync::Semaphore>,
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
    pub fn new(svc: MeshService) -> Arc<Self> {
        Self::new_on(svc, Handle::current())
    }

    /// Like [`Self::new`] but takes an explicit runtime [`Handle`].
    /// Use this when no runtime is entered on the calling thread.
    pub fn new_on(svc: MeshService, rt: Handle) -> Arc<Self> {
        let sender = Arc::new(Self {
            svc: svc.clone(),
            registry: Arc::new(OutgoingVoiceRegistry::default()),
            active: Mutex::new(HashMap::new()),
            rt: rt.clone(),
            retransmit_permits: Arc::new(Semaphore::new(16)),
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
        let message_id = random_message_id()?;

        // Deferred envelope key derivation: if the caller provided a
        // channel PSK (but no pre-derived key) we derive it here with
        // the freshly-assigned message_id.  Pre-derived encryption
        // takes precedence.
        let encryption = match (req.encryption.clone(), &req.channel_psk) {
            (Some(k), _) => Some(k),
            (None, Some(psk)) if !psk.is_empty() => {
                Some(derive_key(psk, message_id, req.from_node_num)?)
            }
            _ => None,
        };

        let chunk_size = req.chunk_size.unwrap_or(MAX_BODY_SIZE);
        let cfg = BuildConfig {
            message_id,
            stream_seq: req.stream_seq,
            codec: req.codec,
            codec_param: req.codec_param,
            chunk_size,
            parity_count: req.parity_count,
            last_in_stream: req.last_in_stream,
            encryption,
            mac_key: req.mac_key.clone(),
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
                mac_key: req.mac_key.clone(),
                pending_retransmit_tasks: 0,
                last_missing_set: Vec::new(),
                identical_nack_count: 0,
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

    /// Read the LoRa modem preset off the service and convert it to
    /// pacing. Falls back to [`ModemPreset::fallback_pacing`] if the
    /// preset hasn't been observed yet (e.g. just-connected radio).
    fn current_pacing(&self) -> Duration {
        let preset = self
            .svc
            .watch_lora_config()
            .borrow()
            .as_ref()
            .and_then(|l| ModemPreset::from_proto(l.modem_preset));
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

        // Check whether someone (a give_up NACK handler) already cleared
        // us out. If so, the terminal status was emitted there.
        let still_active = self.active.lock().contains_key(&message_id);
        if still_active {
            let _ = status_tx.send(SendStatus::Complete { message_id });
            self.cleanup(message_id);
        }
        // Prune expired outgoing entries to keep memory usage low.
        self.registry.prune_expired();
    }

    /// Drop the per-message state on terminal status.
    fn cleanup(&self, message_id: u32) {
        self.active.lock().remove(&message_id);
        self.registry.remove(message_id);
    }

    /// Diagnostic: number of in-flight sends.
    pub fn len(&self) -> usize {
        self.active.lock().len()
    }

    /// Returns `true` if no sends are currently in flight.
    pub fn is_empty(&self) -> bool {
        self.active.lock().is_empty()
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
                warn!(skipped = n, "voice sender NACK listener lagged");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        let Some(sender) = weak.upgrade() else { break };
        if data.portnum != PRIVATE_APP as i32 {
            continue;
        }
        // Cheap version + packet-type filter before locking anything.
        // We don't yet know which `mac_key` to use — the right one is
        // the [`ActiveSend.mac_key`] for this message_id — so peek the
        // message_id from the raw bytes, look it up, then re-parse with
        // the proper key. Frames for messages we know nothing about
        // are dropped without a full parse.
        let Some(message_id) = ChunkHeader::peek_message_id(&data.payload) else {
            continue;
        };
        let entry = {
            let map = sender.active.lock();
            map.get(&message_id)
                .map(|a| (a.status_tx.clone(), a.channel, a.to, a.mac_key.clone()))
        };
        let Some((status_tx, _channel, to, mac_key)) = entry else {
            continue;
        };
        let Ok((header, body)) = ChunkHeader::parse(&data.payload, mac_key.as_deref()) else {
            continue;
        };
        if header.packet_type != PacketType::Nack {
            continue;
        }
        let Ok(nack) = parse_nack_body(&header, body) else {
            continue;
        };

        if nack.give_up {
            info!(message_id = nack.message_id, "voice: receiver gave up");
            let _ = status_tx.send(SendStatus::GaveUp {
                message_id: nack.message_id,
            });
            sender.cleanup(nack.message_id);
            continue;
        }

        // P1a: Give up if message is unrecoverable (>80% loss after 5+ retransmits)
        let loss_ratio = nack.missing.len() as f32 / nack.total_data as f32;
        if loss_ratio > 0.8
            && sender
                .registry
                .retransmit_count(nack.message_id)
                .is_some_and(|r| r >= 5)
        {
            warn!(
                message_id = nack.message_id,
                loss_pct = (loss_ratio * 100.0) as u32,
                retransmits = sender
                    .registry
                    .retransmit_count(nack.message_id)
                    .unwrap_or(0),
                "message unrecoverable (>80% loss), giving up"
            );
            let _ = status_tx.send(SendStatus::GaveUp {
                message_id: nack.message_id,
            });
            sender.cleanup(nack.message_id);
            continue;
        }

        // P2: Detect if receiver is stuck (same chunks NACKed 10+ times).
        // Threshold is high to allow recovery on slow RF links where identical
        // NACKs can occur for extended periods during cooldown windows before
        // a retransmit actually succeeds. 10 rounds ≈ 15 seconds at 1.5s/round.
        {
            let mut map = sender.active.lock();
            if let Some(entry) = map.get_mut(&nack.message_id) {
                if entry.last_missing_set == nack.missing {
                    entry.identical_nack_count += 1;
                    if entry.identical_nack_count >= 10 {
                        warn!(
                            message_id = nack.message_id,
                            missing_count = nack.missing.len(),
                            identical_rounds = entry.identical_nack_count,
                            "receiver stuck (10x identical NACKs), giving up"
                        );
                        drop(map);
                        let _ = status_tx.send(SendStatus::GaveUp {
                            message_id: nack.message_id,
                        });
                        sender.cleanup(nack.message_id);
                        continue;
                    }
                } else {
                    entry.last_missing_set = nack.missing.clone();
                    entry.identical_nack_count = 1;
                }
            }
        }

        let pacing = sender.current_pacing();
        let Some(plan) = sender
            .registry
            .take_retransmit(nack.message_id, &nack.missing, pacing)
        else {
            debug!(
                message_id = nack.message_id,
                requested = nack.missing.len(),
                "voice: retransmit skipped (TTL/budget/cooldown)"
            );
            continue;
        };

        // P1b: Rate-limit concurrent retransmits to prevent task explosion
        {
            let mut map = sender.active.lock();
            if let Some(entry) = map.get_mut(&nack.message_id) {
                if entry.pending_retransmit_tasks >= 2 {
                    debug!(
                        message_id = nack.message_id,
                        pending_tasks = entry.pending_retransmit_tasks,
                        "retransmit already in-flight, skipping NACK round"
                    );
                    continue;
                }
                entry.pending_retransmit_tasks += 1;
            }
        }

        let scheduled: Vec<u8> = plan.iter().map(|(idx, _)| *idx).collect();
        info!(
            message_id = nack.message_id,
            requested = nack.missing.len(),
            scheduled = scheduled.len(),
            "voice: retransmitting"
        );
        let _ = status_tx.send(SendStatus::Retransmitting {
            message_id: nack.message_id,
            chunks: scheduled,
        });

        // Re-enqueue each frame on the paced TX worker. We do this in a
        // detached task so a slow worker doesn't block the NACK
        // listener from processing the next inbound frame. `to` for
        // retransmits is always the *originator's* perspective: if we
        // unicast originally, we still unicast on retransmit; if we
        // broadcast, we still broadcast.
        let permits = Arc::clone(&sender.retransmit_permits);
        let svc = sender.svc.clone();
        let registry = Arc::clone(&sender.registry);
        let message_id = nack.message_id;
        let channel = _channel;
        let want_ack = to.is_some();
        let weak_sender = weak.clone();
        tokio::spawn(async move {
            let _permit = match permits.acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    warn!(message_id, "retransmit semaphore closed");
                    return;
                }
            };
            for batch_idx in 0..plan.len() {
                let (idx, frame) = &plan[batch_idx];
                let r = svc
                    .enqueue_voice_frame_with_id(frame.clone(), channel, to, want_ack, pacing)
                    .await;
                if let Err(e) = r {
                    warn!(message_id, idx, ?e, "voice retransmit enqueue failed");
                    // Clear pending for the un-sent tail of this batch so
                    // a subsequent NACK round can retry them. Without this
                    // the chunks stay stuck in `pending_chunks` forever.
                    for (idx, _) in &plan[(batch_idx + 1)..] {
                        registry.mark_chunk_sent(message_id, *idx);
                    }
                    break;
                }
                // P0: Only mark sent after successful enqueue (was bug: marked before check)
                registry.mark_chunk_sent(message_id, *idx);
            }
            // Decrement pending retransmit task count when done
            if let Some(sender) = weak_sender.upgrade() {
                let mut map = sender.active.lock();
                if let Some(entry) = map.get_mut(&message_id) {
                    entry.pending_retransmit_tasks =
                        entry.pending_retransmit_tasks.saturating_sub(1);
                }
            }
        });
    }
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
