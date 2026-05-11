use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Runtime;

use voicetastic_core::ids::{node_id_to_num, node_num_to_id};
use voicetastic_core::ports::{BROADCAST_ADDR, PRIVATE_APP};
use voicetastic_core::service::MeshService;
use voicetastic_core::voice::{
    AssemblyEvent, ModemPreset as VoiceModemPreset, VoiceAssembler, VoiceDestination, VoiceMessage,
    detect_version,
};

use crate::state::{ChatEntry, Section, SharedState, VoicePayload};

/// Spawn a watcher for a single `tokio::sync::watch` channel that copies the
/// new value into `SharedState` via `apply` and respects the dirty flag at
/// `dirty_for` (if `Some`).
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
    svc: &MeshService,
    shared: Arc<Mutex<SharedState>>,
    ctx: egui::Context,
    assembler: Arc<VoiceAssembler>,
    outgoing: Arc<crate::outgoing::OutgoingVoiceRegistry>,
) {
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
                        if detect_version(&d.payload) != Some(0x01) {
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
        let outgoing = Arc::clone(&outgoing);
        let current_pacing = Arc::clone(&current_pacing);
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
                            // Route the NACK through the voice TX queue
                            // (rather than `send_data` directly) so it
                            // shares the firmware-driven backpressure +
                            // pacing the data frames already use. A long
                            // run of missed chunks otherwise produces a
                            // NACK barrage that overflows the radio's
                            // outbound queue just as badly as the data
                            // burst itself.
                            let svc_nack = svc.clone();
                            let channel = nack.channel;
                            let frame = nack.frame;
                            let pacing = *current_pacing.lock();
                            tokio::spawn(async move {
                                if let Err(e) = svc_nack
                                    .enqueue_voice_frame(
                                        frame,
                                        channel,
                                        Some(to_node),
                                        false,
                                        pacing,
                                    )
                                    .await
                                {
                                    tracing::warn!(?e, "failed to enqueue voice NACK");
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
                                VoiceDestination::Node(d.to)
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
                                    tracing::warn!(
                                        from = %from_id,
                                        ?e,
                                        "voice: chunk rejected"
                                    );
                                }
                                AssemblyEvent::Nack(info) => {
                                    // Selective retransmit: resend exactly
                                    // the data chunks the receiver lists as
                                    // missing. `take_retransmit` enforces
                                    // TTL + budget caps + per-message
                                    // cooldown (so overlapping NACK rounds
                                    // don't pile re-enqueues onto the
                                    // voice TX queue while the previous
                                    // batch is still being paced out).
                                    tracing::debug!(
                                        from = %from_id,
                                        message_id = info.message_id,
                                        missing = info.missing.len(),
                                        give_up = info.give_up,
                                        "voice: NACK received"
                                    );
                                    if info.give_up { continue; }
                                    let pacing = s
                                        .lock()
                                        .lora
                                        .as_ref()
                                        .and_then(|l| {
                                            VoiceModemPreset::from_proto(l.modem_preset)
                                        })
                                        .map(VoiceModemPreset::pacing)
                                        .unwrap_or_else(VoiceModemPreset::fallback_pacing);
                                    let retransmit = outgoing.take_retransmit(
                                        info.message_id,
                                        &info.missing,
                                        pacing,
                                    );
                                    match retransmit.as_ref() {
                                        Some(frames) => tracing::debug!(
                                            message_id = info.message_id,
                                            requested = info.missing.len(),
                                            scheduled = frames.len(),
                                            "voice: retransmit scheduled"
                                        ),
                                        None => tracing::debug!(
                                            message_id = info.message_id,
                                            requested = info.missing.len(),
                                            "voice: retransmit skipped (TTL/budget/cooldown)"
                                        ),
                                    }
                                    if let Some(frames) = retransmit {
                                        let svc = svc.clone();
                                        let outgoing = Arc::clone(&outgoing);
                                        let dest_node = d.from;
                                        let channel = d.channel;
                                        let message_id = info.message_id;
                                        tokio::spawn(async move {
                                            // Serialise retransmits through the worker the
                                            // same way the initial burst does: each call
                                            // awaits the worker's actual `send_data`, so
                                            // we never push the next retransmit frame
                                            // until the previous one has left the
                                            // mpsc. This caps the voice TX queue's
                                            // contribution from a retransmit task at 1
                                            // slot regardless of how many chunks the
                                            // peer asked for.
                                            //
                                            // `mark_chunk_sent` clears the pending flag
                                            // so a *later* NACK round can request the
                                            // same chunk again if it's still missing,
                                            // while the dedup in `take_retransmit`
                                            // prevents two overlapping NACK rounds
                                            // from re-enqueuing chunks still in flight.
                                            for (idx, frame) in frames {
                                                let r = svc
                                                    .enqueue_voice_frame_with_id(
                                                        frame,
                                                        channel,
                                                        Some(dest_node),
                                                        false,
                                                        pacing,
                                                    )
                                                    .await;
                                                outgoing.mark_chunk_sent(message_id, idx);
                                                if let Err(e) = r {
                                                    tracing::warn!(
                                                        ?e,
                                                        "voice retransmit failed"
                                                    );
                                                    break;
                                                }
                                            }
                                        });
                                    }
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
        .and_then(|l| VoiceModemPreset::from_proto(l.modem_preset))
        .map(VoiceModemPreset::pacing)
        .unwrap_or_else(VoiceModemPreset::fallback_pacing);
    *current_pacing.lock() = pacing;
    let nack_window = (pacing * 4).max(Duration::from_millis(2_000));
    // In-place mutation so we don't stomp on `message_timeout` /
    // `completion_memory` set by `VoicetasticApp::apply_voice_settings`.
    assembler.update_config(|cfg| {
        cfg.nack_window = nack_window;
    });
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
        VoiceDestination::Node(n) => n,
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

/// Best-effort 20 ms-frame estimate for an Opus stream produced by
/// `audio::Recorder` (length-prefixed packets, one packet per 20 ms).
#[allow(dead_code)]
fn estimate_opus_duration_ms(stream: &[u8]) -> u32 {
    let mut i = 0;
    let mut packets: u32 = 0;
    while i + 2 <= stream.len() {
        let len = u16::from_be_bytes([stream[i], stream[i + 1]]) as usize;
        i += 2;
        if i + len > stream.len() {
            break;
        }
        i += len;
        packets += 1;
    }
    packets * 20
}
