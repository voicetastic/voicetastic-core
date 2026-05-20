use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Runtime;
use tracing::warn;

use voicetastic_core::MeshtasticService;
use voicetastic_core::ids::{node_id_to_num, node_num_to_id};
use voicetastic_core::meshtastic::service::modem_preset_from_proto;
use voicetastic_core::node::NodeId;
use voicetastic_core::ports::{BROADCAST_ADDR, PRIVATE_APP};
use voicetastic_core::voice::{
    AssemblyEvent, ModemPreset as VoiceModemPreset, PROTOCOL_VERSION, VoiceAssembler,
    VoiceDestination, VoiceMessage, detect_version,
};

use crate::state::{ChatEntry, Section, SharedState, VoicePayload};

/// Spawn a watcher for a single `tokio::sync::watch` channel that copies the
/// new value into `SharedState` via `apply` and respects the dirty flag at
/// `dirty_for` (if `Some`).
///
/// Race-freedom note: the `if !st.dirty.contains(...)` check and the
/// corresponding `st.dirty.insert(...)` on the UI side both run while
/// holding `SharedState`'s mutex, so check-then-write is atomic and
/// in-progress edits cannot be clobbered by a watcher write that landed
/// between the user's read and write. Do not move the dirty check out of
/// the macro's `$state` critical section.
macro_rules! spawn_watch {
    ($rt:expr, $rx:expr, $shared:expr, $ctx:expr, |$value:ident, $state:ident| $apply:block) => {{
        let mut rx = $rx;
        let s = Arc::clone(&$shared);
        let c = $ctx.clone();
        $rt.spawn(async move {
            while rx.changed().await.is_ok() {
                let $value = rx.borrow_and_update().clone();
                {
                    let mut $state = s.lock();
                    $apply;
                }
                c.request_repaint();
            }
        });
    }};
}

pub fn spawn_watchers(
    rt: &Runtime,
    svc: &MeshtasticService,
    shared: Arc<Mutex<SharedState>>,
    ctx: egui::Context,
    assembler: Arc<VoiceAssembler>,
) {
    // Pairing-prompt forwarder (Linux only). Takes ownership of the
    // BlueZ Agent1 receiver and stuffs each prompt into `SharedState`
    // so the modal in `app.rs` can render it. If a previous prompt is
    // still pending we reject the older one (BlueZ tolerates concurrent
    // pairings poorly anyway).
    #[cfg(target_os = "linux")]
    {
        let svc = svc.clone();
        let shared = Arc::clone(&shared);
        let ctx_clone = ctx.clone();
        rt.spawn(async move {
            let mut rx = match svc.pairing_prompts().await {
                Some(rx) => rx,
                None => return,
            };
            while let Some(prompt) = rx.recv().await {
                let mut st = shared.lock();
                // Cancel any previous in-flight prompt.
                if let Some(mut prev) = st.pending_pairing.take()
                    && let Some(reply) = prev.reply.take()
                {
                    let _ = reply.send(voicetastic_core::pairing::PairingResponse::Cancel);
                }
                st.pending_pairing = Some(crate::state::PendingPairing {
                    address: prompt.address,
                    kind: prompt.kind,
                    reply: Some(prompt.reply),
                    input: String::new(),
                });
                drop(st);
                ctx_clone.request_repaint();
            }
        });
    }

    spawn_watch!(rt, svc.watch_state(), shared, ctx, |v, st| {
        st.conn_state = v;
    });
    spawn_watch!(rt, svc.watch_my_info(), shared, ctx, |v, st| {
        st.my_info = v;
    });
    spawn_watch!(rt, svc.watch_nodes(), shared, ctx, |v, st| {
        st.nodes = v;
    });

    spawn_watch!(rt, svc.watch_lora_config(), shared, ctx, |v, st| {
        if !st.dirty.contains(&Section::Lora) {
            st.lora = v;
        }
    });
    // Re-apply the voice assembler config every time the LoRa preset
    // changes so the NACK window scales with on-air time. Otherwise the
    // default 1.5 s window fires NACKs for chunks that are still being
    // transmitted on slow presets (LongFast pacing is ~900 ms, so a
    // 30-chunk burst with a couple of CSMA backoffs easily produces
    // >1.5 s gaps and the receiver-side state machine starts demanding
    // retransmits of frames the sender hasn't even queued yet).
    // Tracks the current modem preset's recommended inter-frame pacing
    // so the NACK fan-out can hand a sensible `pacing` value to the
    // voice TX queue (NACK frames also need to be backpressured against
    // the firmware's outbound queue, otherwise a long burst of missed
    // chunks produces a NACK barrage that can itself overflow the radio).
    let current_pacing = Arc::new(Mutex::new(VoiceModemPreset::fallback_pacing()));
    {
        let mut rx = svc.watch_lora_config();
        let assembler = Arc::clone(&assembler);
        let current_pacing = Arc::clone(&current_pacing);
        rt.spawn(async move {
            // Apply once with whatever we already know.
            apply_lora_to_assembler(&assembler, rx.borrow().as_ref(), &current_pacing);
            while rx.changed().await.is_ok() {
                let cfg = rx.borrow_and_update().clone();
                apply_lora_to_assembler(&assembler, cfg.as_ref(), &current_pacing);
            }
        });
    }
    spawn_watch!(rt, svc.watch_device_config(), shared, ctx, |v, st| {
        if !st.dirty.contains(&Section::Device) {
            st.device = v;
        }
    });
    spawn_watch!(rt, svc.watch_position_config(), shared, ctx, |v, st| {
        if !st.dirty.contains(&Section::Position) {
            st.position = v;
        }
    });
    spawn_watch!(rt, svc.watch_power_config(), shared, ctx, |v, st| {
        if !st.dirty.contains(&Section::Power) {
            st.power = v;
        }
    });
    spawn_watch!(rt, svc.watch_network_config(), shared, ctx, |v, st| {
        if !st.dirty.contains(&Section::Network) {
            st.network = v;
        }
    });
    spawn_watch!(rt, svc.watch_display_config(), shared, ctx, |v, st| {
        if !st.dirty.contains(&Section::Display) {
            st.display = v;
        }
    });
    spawn_watch!(rt, svc.watch_bluetooth_config(), shared, ctx, |v, st| {
        if !st.dirty.contains(&Section::Bluetooth) {
            st.bluetooth = v;
        }
    });
    spawn_watch!(rt, svc.watch_owner(), shared, ctx, |v, st| {
        if !st.dirty.contains(&Section::Owner) {
            st.owner = v;
        }
    });
    spawn_watch!(rt, svc.watch_metadata(), shared, ctx, |v, st| {
        st.metadata = v;
    });
    spawn_watch!(rt, svc.watch_channels(), shared, ctx, |v, st| {
        // Replace only channels that aren't being edited.
        let kept: Vec<_> = st
            .channels
            .iter()
            .filter(|c| st.dirty.contains(&Section::Channel(c.index)))
            .cloned()
            .collect();
        let mut next: Vec<_> = v
            .into_iter()
            .filter(|c| !st.dirty.contains(&Section::Channel(c.index)))
            .collect();
        next.extend(kept);
        next.sort_by_key(|c| c.index);
        st.channels = next;
    });

    // Incoming text -> chat log
    {
        let mut rx = svc.subscribe_text();
        let s = Arc::clone(&shared);
        let c = ctx.clone();
        rt.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        s.lock().push_chat(ChatEntry {
                            text: msg.text.clone(),
                            rx_time: msg.rx_time,
                            outgoing: false,
                            channel: msg.channel,
                            from_num: msg.from,
                            to_num: msg.to,
                            voice: None,
                            outgoing_voice_id: None,
                            inbound_voice_id: None,
                        });
                        c.request_repaint();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "text broadcast lagged");
                    }
                }
            }
        });
    }

    // config_complete -> clear all dirty + status
    {
        let mut rx = svc.subscribe_config_complete();
        let s = Arc::clone(&shared);
        let c = ctx.clone();
        rt.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(_) => {
                        let mut st = s.lock();
                        st.dirty.clear();
                        st.config_status = Some("Config received".into());
                        drop(st);
                        c.request_repaint();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(_) => {}
                }
            }
        });
    }

    // Inbound voice (PRIVATE_APP) -> reassemble and post a chat notification
    // when a message completes (or partially completes on timeout). NACKs
    // emitted by `assembler.tick()` are forwarded back to the originating
    // sender so its retransmit loop (when present) can close the gap.
    //
    // Split into two tasks to insulate the assembler from broadcast lag:
    //
    //   broadcast::recv → mpsc::send (cheap, never blocks)
    //                  → mpsc::recv → assembler.accept (slow path)
    //
    // The forwarder does nothing but drain the broadcast and push onto
    // the mpsc, so even a slow assembler tick or a contended
    // `SharedState` lock can't cause `RecvError::Lagged` and silently
    // drop voice chunks.
    {
        let mut rx = svc.subscribe_data();
        // 512 is comfortably larger than any single message's frame
        // count (parity scaling caps total frames at 255+128=383) so a
        // back-to-back burst from a fast sender can land entirely in
        // the queue even if the assembler stalls briefly.
        let (q_tx, mut q_rx) =
            tokio::sync::mpsc::channel::<voicetastic_core::service::IncomingData>(512);

        // Forwarder: pure broadcast → mpsc drain. Filters out non-voice
        // ports and bad protocol versions before queueing so the
        // assembler task never wakes up for noise.
        rt.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(d) => {
                        if d.portnum != PRIVATE_APP as i32 {
                            continue;
                        }
                        if detect_version(&d.payload) != Some(PROTOCOL_VERSION) {
                            continue;
                        }
                        if q_tx.send(d).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "voice broadcast lagged");
                    }
                }
            }
        });

        let s = Arc::clone(&shared);
        let c = ctx.clone();
        let assembler = Arc::clone(&assembler);
        let svc = svc.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let out = assembler.tick();
                        for completed in out.finalized {
                            push_voice_entry(&s, &c, &completed);
                        }
                        for nack in out.nacks {
                            let to_node = match voicetastic_core::ids::node_id_to_num(&nack.from) {
                                Ok(n) => n,
                                Err(e) => {
                                    tracing::warn!(from = %nack.from, ?e, "skip NACK: bad node id");
                                    continue;
                                }
                            };
                            tracing::debug!(
                                to = %nack.from,
                                message_id = nack.message_id,
                                missing = nack.missing_count,
                                round = nack.round,
                                "voice: emitting NACK"
                            );
                            // Send the NACK via `send_data` directly (not
                            // through the voice TX queue) so it bypasses
                            // any queue congestion from an ongoing
                            // outbound voice burst. With a NACK window
                            // of several seconds the emission rate is
                            // < 1 NACK/s — far too low to overwhelm the
                            // firmware's TX queue.
                            let svc_nack = svc.clone();
                            let channel = nack.channel;
                            let frame = nack.frame;
                            tokio::spawn(async move {
                                if let Err(e) = svc_nack
                                    .send_data(
                                        PRIVATE_APP as i32,
                                        frame,
                                        channel,
                                        Some(to_node),
                                        false,
                                        false, // want_response
                                    )
                                    .await
                                {
                                    tracing::warn!(?e, "failed to send voice NACK");
                                }
                            });
                        }
                    }
                    msg = q_rx.recv() => match msg {
                        Some(d) => {
                            let from_id = node_num_to_id(d.from);
                            let to = if d.to == BROADCAST_ADDR {
                                VoiceDestination::Broadcast
                            } else {
                                VoiceDestination::Node(NodeId::from_u32(d.to))
                            };
                            match assembler.accept(&from_id, to, d.channel, &d.payload) {
                                AssemblyEvent::Complete(m) => push_voice_entry(&s, &c, &m),
                                AssemblyEvent::Pending {
                                    message_id,
                                    from,
                                    received_data,
                                    total_data,
                                    channel,
                                } => {
                                    upsert_inbound_voice_progress(
                                        &s,
                                        &c,
                                        &from,
                                        message_id,
                                        received_data,
                                        total_data,
                                        channel,
                                        d.to,
                                    );
                                }
                                AssemblyEvent::Duplicate => {
                                    tracing::debug!(
                                        from = %from_id,
                                        "voice: duplicate chunk ignored"
                                    );
                                }
                                AssemblyEvent::Rejected(e) => {
                                    // `Blacklisted` is the expected tail
                                    // event for chunks arriving after the
                                    // message already completed (sender's
                                    // FEC + retransmit budget keeps
                                    // transmitting briefly past the last
                                    // needed shard). Demote to debug so
                                    // it doesn't spam WARN; everything
                                    // else still warns.
                                    use voicetastic_core::voice::VoiceError;
                                    if matches!(e, VoiceError::Blacklisted) {
                                        tracing::debug!(
                                            from = %from_id,
                                            "voice: late chunk after completion (blacklisted)"
                                        );
                                    } else {
                                        tracing::warn!(
                                            from = %from_id,
                                            ?e,
                                            "voice: chunk rejected"
                                        );
                                    }
                                }
                                AssemblyEvent::Nack(info) => {
                                    // Inbound NACKs targeting our own
                                    // outgoing messages are serviced
                                    // transparently by VoiceSender's
                                    // internal NACK listener. The
                                    // watcher only logs them here for
                                    // visibility; retransmits, cooldown,
                                    // TTL, and budget caps all live in
                                    // core.
                                    tracing::debug!(
                                        from = %from_id,
                                        message_id = info.message_id,
                                        missing = info.missing.len(),
                                        give_up = info.give_up,
                                        "voice: NACK received (handled by VoiceSender)"
                                    );
                                }
                            }
                        }
                        None => break,
                    },
                }
            }
        });
    }
}

/// Reconfigure the voice assembler when the active LoRa preset changes.
/// We scale `nack_window` to ≈ 4× the preset's recommended inter-frame
/// pacing (clamped to a minimum) so the receiver doesn't fire NACK
/// rounds for chunks that are still being paced out by the sender.
fn apply_lora_to_assembler(
    assembler: &VoiceAssembler,
    lora: Option<&voicetastic_core::proto::config::LoRaConfig>,
    current_pacing: &Mutex<Duration>,
) {
    let pacing = lora
        .and_then(|l| modem_preset_from_proto(l.modem_preset))
        .map(VoiceModemPreset::pacing)
        .unwrap_or_else(VoiceModemPreset::fallback_pacing);
    *current_pacing.lock() = pacing;
    let nack_window = (pacing * 4).max(Duration::from_millis(2_000));
    // In-place mutation so we don't stomp on `message_timeout` /
    // `completion_memory` set by `VoicetasticApp::apply_voice_settings`.
    // Resync the round cap so the new (often larger) `nack_window`
    // doesn't blow past the configured `message_timeout`.
    if let Err(e) = assembler.update_config(|cfg| {
        cfg.nack_window = nack_window;
        cfg.sync_nack_cap_to_timeout();
    }) {
        warn!("Failed to update assembler nack_window: {}", e);
    }
}

fn push_voice_entry(s: &Arc<Mutex<SharedState>>, c: &egui::Context, msg: &VoiceMessage) {
    let label = if msg.is_complete {
        format!(
            "🎙 voice message ({} bytes, {} chunks)",
            msg.audio.len(),
            msg.total_data
        )
    } else {
        format!(
            "🎙 voice message (partial: {}/{} chunks, {} bytes)",
            msg.received_data,
            msg.total_data,
            msg.audio.len()
        )
    };
    let duration_ms = crate::audio::payload_duration_ms(&msg.audio, msg.codec, msg.codec_param);
    let voice = if msg.is_complete && msg.codec.is_known() && !msg.audio.is_empty() {
        Some(VoicePayload {
            codec: msg.codec,
            codec_param: msg.codec_param,
            bytes: msg.audio.clone(),
            duration_ms,
        })
    } else {
        None
    };
    let from_num = node_id_to_num(&msg.from).unwrap_or(0);
    let to_num = match msg.to {
        VoiceDestination::Broadcast => BROADCAST_ADDR,
        VoiceDestination::Node(n) => n.as_u32(),
    };
    let mut st = s.lock();
    // If a "receiving …" placeholder was already pushed for this
    // (from, message_id), upgrade it in place so the chat doesn't grow
    // a second entry for every voice message.
    if let Some(entry) = st.chat_log.iter_mut().rev().find(|e| {
        !e.outgoing && e.from_num == from_num && e.inbound_voice_id == Some(msg.message_id)
    }) {
        // Never downgrade an already-completed entry back to "partial".
        // If a late finalize lands (stray chunk after blacklist expiry,
        // duplicate broadcast, etc.) keep the original complete payload
        // so the user doesn't see their playable message turn into a
        // sad partial label.
        let already_complete = entry.voice.is_some();
        if !already_complete || msg.is_complete {
            entry.text = label;
            entry.voice = voice;
            entry.channel = msg.channel;
            entry.to_num = to_num;
        }
    } else {
        st.push_chat(ChatEntry {
            text: label,
            rx_time: 0,
            outgoing: false,
            channel: msg.channel,
            from_num,
            to_num,
            voice,
            outgoing_voice_id: None,
            inbound_voice_id: Some(msg.message_id),
        });
    }
    drop(st);
    c.request_repaint();
}

#[allow(clippy::too_many_arguments)]
fn upsert_inbound_voice_progress(
    s: &Arc<Mutex<SharedState>>,
    c: &egui::Context,
    from: &str,
    message_id: u32,
    received_data: u8,
    total_data: u8,
    channel: u32,
    to_num: u32,
) {
    let from_num = node_id_to_num(from).unwrap_or(0);
    let label = format!("🎙 receiving voice ({received_data}/{total_data} chunks)…");
    let mut st = s.lock();
    if let Some(entry) =
        st.chat_log.iter_mut().rev().find(|e| {
            !e.outgoing && e.from_num == from_num && e.inbound_voice_id == Some(message_id)
        })
    {
        entry.text = label;
    } else {
        st.push_chat(ChatEntry {
            text: label,
            rx_time: 0,
            outgoing: false,
            channel,
            from_num,
            to_num,
            voice: None,
            outgoing_voice_id: None,
            inbound_voice_id: Some(message_id),
        });
    }
    drop(st);
    c.request_repaint();
}
