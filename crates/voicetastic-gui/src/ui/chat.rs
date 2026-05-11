use std::collections::BTreeSet;
use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;

use voicetastic_core::ports::BROADCAST_ADDR;
use voicetastic_core::proto::{
    Channel, NodeInfo, channel::Role, config::LoRaConfig, config::lo_ra_config::ModemPreset,
};
use voicetastic_core::voice::{
    BuildConfig, MAX_BODY_SIZE, MAX_PARITY_PER_MESSAGE, ModemPreset as VoiceModemPreset,
    VoiceCodec, build_message, random_message_id,
};

use crate::app::{PlaybackSource, VoicetasticApp};
use crate::audio::{self, PlaybackHandle, RecordedClip, Recorder};
use crate::state::{ChatEntry, SharedState, VoicePayload};

/// Voice-message compose state machine driven by the Chat tab UI.
///
/// Variants:
/// - `Idle`: no recording in progress, mic icon shown.
/// - `Recording`: cpal stream running, timer ticking, Stop button shown.
/// - `Preview`: clip captured, user can listen / delete / send.
#[derive(Default)]
pub enum VoiceCompose {
    #[default]
    Idle,
    Recording(Recorder),
    Preview {
        clip: RecordedClip,
    },
}

/// Default name the Meshtastic firmware uses for the primary channel when
/// the user hasn't overridden it. Derived from the active LoRa modem preset
/// (e.g. `LongFast`, `MediumSlow`, ...).
fn primary_default_name(lora: Option<&LoRaConfig>) -> String {
    let preset = lora
        .and_then(|l| ModemPreset::try_from(l.modem_preset).ok())
        .unwrap_or(ModemPreset::LongFast);
    let raw = preset.as_str_name();
    let mut out = String::with_capacity(raw.len());
    for word in raw.split('_') {
        let mut chars = word.chars();
        if let Some(c) = chars.next() {
            out.push(c.to_ascii_uppercase());
            for c in chars {
                out.push(c.to_ascii_lowercase());
            }
        }
    }
    out
}

fn channel_label(ch: &Channel, lora: Option<&LoRaConfig>) -> String {
    let name = ch.settings.as_ref().map(|s| s.name.trim()).unwrap_or("");
    if !name.is_empty() {
        return name.to_string();
    }
    if ch.index == 0 {
        return primary_default_name(lora);
    }
    format!("Channel {}", ch.index)
}

fn channel_label_for_index(channels: &[Channel], lora: Option<&LoRaConfig>, idx: u32) -> String {
    if let Some(ch) = channels.iter().find(|c| c.index as u32 == idx) {
        return channel_label(ch, lora);
    }
    if idx == 0 {
        primary_default_name(lora)
    } else {
        format!("Channel {idx}")
    }
}

fn is_active(ch: &Channel) -> bool {
    if ch.index == 0 {
        return true;
    }
    if Role::try_from(ch.role) != Ok(Role::Disabled) {
        return true;
    }
    ch.settings
        .as_ref()
        .map(|s| !s.name.trim().is_empty())
        .unwrap_or(false)
}

/// Friendly display name for a node: long_name → short_name → !id → hex.
fn node_display_name(node: Option<&NodeInfo>, num: u32) -> String {
    if let Some(n) = node
        && let Some(u) = n.user.as_ref()
    {
        if !u.long_name.is_empty() {
            return u.long_name.clone();
        }
        if !u.short_name.is_empty() {
            return u.short_name.clone();
        }
        if !u.id.is_empty() {
            return u.id.clone();
        }
    }
    format!("!{num:08x}")
}

/// One conversation thread (broadcast on a channel, or a DM with a node).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Thread {
    /// Broadcast traffic on the given channel index.
    Broadcast(u32),
    /// Direct messages with the given node num (regardless of channel).
    Direct(u32),
}

/// Which thread (if any) does this entry belong to, given our own node num?
fn entry_thread(e: &ChatEntry, my_num: Option<u32>) -> Option<Thread> {
    let my = my_num.unwrap_or(0);
    if e.outgoing {
        // Outgoing entries carry the destination we picked at send time.
        if e.to_num == BROADCAST_ADDR || e.to_num == 0 {
            Some(Thread::Broadcast(e.channel))
        } else {
            Some(Thread::Direct(e.to_num))
        }
    } else if e.to_num == BROADCAST_ADDR {
        Some(Thread::Broadcast(e.channel))
    } else if e.to_num == my {
        // DM addressed to us — bucket by the remote peer.
        Some(Thread::Direct(e.from_num))
    } else {
        // Some other DM we just happened to overhear; ignore.
        None
    }
}

pub fn show(app: &mut VoicetasticApp, ui: &mut egui::Ui) {
    ui.heading("Text Chat");
    ui.separator();

    // Snapshot shared state once per frame.
    let (channels, lora, log, nodes, my_num) = {
        let st = app.shared.lock();
        (
            st.channels.clone(),
            st.lora.clone(),
            st.chat_log.clone(),
            st.nodes.clone(),
            st.my_info.as_ref().map(|m| m.my_node_num),
        )
    };

    // Active channels (for the broadcast room dropdown).
    let mut bcast_indices: BTreeSet<u32> = BTreeSet::new();
    bcast_indices.insert(0);
    bcast_indices.insert(app.chat_channel);
    for c in &channels {
        if is_active(c) {
            bcast_indices.insert(c.index as u32);
        }
    }
    for e in &log {
        if let Some(Thread::Broadcast(ch)) = entry_thread(e, my_num) {
            bcast_indices.insert(ch);
        }
    }

    // Sorted node list for the DM dropdown.
    let mut node_choices: Vec<(u32, String)> = nodes
        .values()
        .filter(|n| Some(n.num) != my_num && n.num != BROADCAST_ADDR)
        .map(|n| (n.num, node_display_name(Some(n), n.num)))
        .collect();
    node_choices.sort_by_key(|a| a.1.to_lowercase());

    // Mode selector: broadcast on a channel, or DM with a node.
    ui.horizontal(|ui| {
        let mut is_broadcast = app.chat_dest.is_none();
        if ui
            .radio_value(&mut is_broadcast, true, "Broadcast")
            .clicked()
        {
            app.chat_dest = None;
        }
        if ui.radio_value(&mut is_broadcast, false, "Direct").clicked()
            && app.chat_dest.is_none()
            && let Some((num, _)) = node_choices.first()
        {
            app.chat_dest = Some(*num);
        }

        if app.chat_dest.is_none() {
            ui.label("Channel:");
            let selected = channel_label_for_index(&channels, lora.as_ref(), app.chat_channel);
            egui::ComboBox::from_id_salt("chat_channel_select")
                .selected_text(format!("{selected} (#{})", app.chat_channel))
                .show_ui(ui, |ui| {
                    for idx in &bcast_indices {
                        let label = format!(
                            "{} (#{idx})",
                            channel_label_for_index(&channels, lora.as_ref(), *idx)
                        );
                        ui.selectable_value(&mut app.chat_channel, *idx, label);
                    }
                });
        } else if let Some(dest_num) = app.chat_dest {
            ui.label("Node:");
            let dest_label = node_choices
                .iter()
                .find(|(n, _)| *n == dest_num)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| format!("!{dest_num:08x}"));
            egui::ComboBox::from_id_salt("chat_dest_select")
                .selected_text(dest_label)
                .show_ui(ui, |ui| {
                    if node_choices.is_empty() {
                        ui.label("(no nodes known yet)");
                    }
                    for (num, name) in &node_choices {
                        ui.selectable_value(&mut app.chat_dest, Some(*num), name.clone());
                    }
                });
        }
    });
    ui.separator();

    // Active thread.
    let active = match app.chat_dest {
        None => Thread::Broadcast(app.chat_channel),
        Some(num) => Thread::Direct(num),
    };

    // Auto-clear playback handle when finished so the inline player
    // disappears and the next ▶ Play click starts fresh.
    if let Some(h) = app.voice_playback.as_ref()
        && h.is_finished()
    {
        app.voice_playback = None;
        app.playback_source = None;
    }

    // Messages for the active thread.
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .max_height(ui.available_height() - 80.0)
        .show(ui, |ui| {
            let mut any = false;
            let mut play_request: Option<(usize, Vec<u8>, VoiceCodec, u8)> = None;
            let mut stop_request = false;
            for (idx, entry) in log.iter().enumerate() {
                if entry_thread(entry, my_num) != Some(active) {
                    continue;
                }
                any = true;
                let prefix = if entry.outgoing {
                    "→ You".to_string()
                } else {
                    let node = nodes.get(&entry.from_num);
                    node_display_name(node, entry.from_num)
                };
                let is_playing = app.playback_source == Some(PlaybackSource::LogEntry(idx))
                    && app.voice_playback.is_some();
                ui.horizontal(|ui| {
                    ui.label(format!("{prefix}: {}", entry.text));
                    if let Some(v) = entry.voice.as_ref()
                        && audio::is_available()
                        && matches!(v.codec, VoiceCodec::Opus | VoiceCodec::Codec2)
                    {
                        if is_playing {
                            if inline_player(ui, app.voice_playback.as_ref()) {
                                stop_request = true;
                            }
                        } else if ui.small_button("▶ Play").clicked() {
                            play_request = Some((idx, v.bytes.clone(), v.codec, v.codec_param));
                        }
                    }
                });
            }
            if !any {
                ui.weak("(no messages in this conversation yet)");
            }
            if stop_request {
                if let Some(h) = app.voice_playback.take() {
                    h.stop();
                }
                app.playback_source = None;
            }
            if let Some((idx, bytes, codec, codec_param)) = play_request {
                start_playback(
                    app,
                    &bytes,
                    codec,
                    codec_param,
                    PlaybackSource::LogEntry(idx),
                );
            }
        });

    // Voice composer + text input row.
    ui.separator();
    voice_composer(app, ui, &nodes);
    if let Some(msg) = app.chat_status.clone() {
        ui.horizontal(|ui| {
            ui.colored_label(egui::Color32::from_rgb(220, 100, 100), &msg);
            if ui.small_button("✕").clicked() {
                app.chat_status = None;
            }
        });
    }
    ui.horizontal(|ui| {
        let resp = ui.text_edit_singleline(&mut app.chat_input);
        let send = (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
            || ui.button("Send").clicked();
        if send {
            let text = app.chat_input.clone();
            if !text.is_empty() {
                app.chat_input.clear();
                let svc = app.service.clone();
                let dest = app.chat_dest;
                // For DMs the channel index is the recipient node's home
                // channel (defaulting to 0 if unknown).
                let ch = match dest {
                    None => app.chat_channel,
                    Some(num) => nodes.get(&num).map(|n| n.channel).unwrap_or(0),
                };
                let to_num = dest.unwrap_or(BROADCAST_ADDR);
                let shared = Arc::clone(&app.shared);
                app.rt.spawn(async move {
                    match svc.send_text(&text, ch, dest).await {
                        Ok(_id) => {
                            shared.lock().push_chat(ChatEntry {
                                text,
                                rx_time: 0,
                                outgoing: true,
                                channel: ch,
                                from_num: 0,
                                to_num,
                                voice: None,
                                outgoing_voice_id: None,
                                inbound_voice_id: None,
                            });
                        }
                        Err(e) => {
                            shared.lock().status_msg = Some(format!("Send failed: {e}"));
                        }
                    }
                });
            }
        }
    });
}

/// Route the destination channel/node for the active thread to a
/// `(channel, dest)` pair compatible with `MeshService::send_voice`.
fn resolve_destination(
    app: &VoicetasticApp,
    nodes: &std::collections::HashMap<u32, voicetastic_core::proto::NodeInfo>,
) -> (u32, Option<u32>) {
    match app.chat_dest {
        None => (app.chat_channel, None),
        Some(num) => {
            let ch = nodes.get(&num).map(|n| n.channel).unwrap_or(0);
            (ch, Some(num))
        }
    }
}

fn start_playback(
    app: &mut VoicetasticApp,
    bytes: &[u8],
    codec: VoiceCodec,
    codec_param: u8,
    source: PlaybackSource,
) {
    // Drop any in-flight playback first so the new clip starts cleanly.
    if let Some(h) = app.voice_playback.take() {
        h.stop();
    }
    match audio::play_clip(bytes, codec, codec_param) {
        Ok(handle) => {
            app.voice_playback = Some(handle);
            app.playback_source = Some(source);
        }
        Err(e) => {
            app.chat_status = Some(format!("Playback failed: {e}"));
            app.playback_source = None;
        }
    }
}

/// Tiny inline transport widget rendered next to a message that's
/// currently playing. Returns `true` if the user clicked Stop.
fn inline_player(ui: &mut egui::Ui, handle: Option<&PlaybackHandle>) -> bool {
    let (elapsed, total) = handle
        .map(|h| h.progress())
        .unwrap_or((std::time::Duration::ZERO, std::time::Duration::ZERO));
    let total_s = total.as_secs_f32().max(0.001);
    let frac = (elapsed.as_secs_f32() / total_s).clamp(0.0, 1.0);
    ui.add(
        egui::ProgressBar::new(frac)
            .desired_width(140.0)
            .text(format!(
                "{:.1} / {:.1} s",
                elapsed.as_secs_f32(),
                total.as_secs_f32(),
            )),
    );
    // Keep the progress bar smooth without spinning the CPU.
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(80));
    ui.small_button("⏹").clicked()
}

/// Idle / Recording / Preview UI rendered above the text input.
fn voice_composer(
    app: &mut VoicetasticApp,
    ui: &mut egui::Ui,
    nodes: &std::collections::HashMap<u32, voicetastic_core::proto::NodeInfo>,
) {
    let max_secs = app.settings.voice_max_secs();
    // Take ownership of the state so we can transition without juggling
    // borrow rules; we put the (possibly-new) state back at the end.
    let prev = std::mem::take(&mut app.voice_compose);
    let next = match prev {
        VoiceCompose::Idle => render_idle(app, ui, max_secs),
        VoiceCompose::Recording(rec) => render_recording(app, ui, rec, max_secs),
        VoiceCompose::Preview { clip } => render_preview(app, ui, nodes, clip),
    };
    app.voice_compose = next;
}

fn render_idle(app: &mut VoicetasticApp, ui: &mut egui::Ui, max_secs: u32) -> VoiceCompose {
    ui.horizontal(|ui| {
        let enabled = audio::is_available();
        let resp = ui.add_enabled(enabled, egui::Button::new("🎙 Record voice"));
        if !enabled {
            resp.on_hover_text("Rebuild with `--features audio` to enable voice messages.");
        } else if resp.clicked() {
            let (codec, codec_param) = app.outgoing_voice_codec();
            match Recorder::start(max_secs, codec, codec_param) {
                Ok(rec) => {
                    return VoiceCompose::Recording(rec);
                }
                Err(e) => {
                    app.chat_status = Some(format!("Mic error: {e}"));
                }
            }
        }
        ui.weak(format!("(max {max_secs} s)"));
        VoiceCompose::Idle
    })
    .inner
}

fn render_recording(
    app: &mut VoicetasticApp,
    ui: &mut egui::Ui,
    rec: Recorder,
    max_secs: u32,
) -> VoiceCompose {
    let elapsed = rec.elapsed().as_secs_f32().min(max_secs as f32);
    let mut next = VoiceCompose::Recording(rec);
    ui.horizontal(|ui| {
        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), "● Recording");
        ui.add(
            egui::ProgressBar::new(elapsed / max_secs as f32)
                .desired_width(160.0)
                .text(format!("{:.1} / {} s", elapsed, max_secs)),
        );
        let stop = ui.button("⏹ Stop").clicked();
        let cancel = ui.button("✖ Cancel").clicked();
        // Auto-stop when the cap elapses.
        let auto_stop = elapsed >= max_secs as f32;
        if stop || auto_stop {
            if let VoiceCompose::Recording(r) = std::mem::replace(&mut next, VoiceCompose::Idle) {
                match r.finish() {
                    Ok(clip) => {
                        next = VoiceCompose::Preview { clip };
                    }
                    Err(e) => {
                        app.chat_status = Some(format!("Recording failed: {e}"));
                        next = VoiceCompose::Idle;
                    }
                }
            }
        } else if cancel {
            // Drop the recorder; its `Drop` impl will signal the capture
            // thread to stop.
            next = VoiceCompose::Idle;
        }
    });
    // Egui doesn't auto-repaint while a non-input task is running; nudge it
    // so the timer/progress-bar update smoothly.
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(100));
    next
}

fn render_preview(
    app: &mut VoicetasticApp,
    ui: &mut egui::Ui,
    nodes: &std::collections::HashMap<u32, voicetastic_core::proto::NodeInfo>,
    clip: RecordedClip,
) -> VoiceCompose {
    let mut send_now = false;
    let mut delete_now = false;
    let mut play_clicked = false;
    let mut stop_clicked = false;
    let is_playing =
        app.playback_source == Some(PlaybackSource::Preview) && app.voice_playback.is_some();
    ui.horizontal(|ui| {
        ui.label(format!(
            "🎙 Preview ({:.1}s, {} bytes)",
            clip.duration.as_secs_f32(),
            clip.payload.len()
        ));
        if is_playing {
            if inline_player(ui, app.voice_playback.as_ref()) {
                stop_clicked = true;
            }
        } else if ui.button("▶ Play").clicked() {
            play_clicked = true;
        }
        if ui.button("🗑 Delete").clicked() {
            delete_now = true;
        }
        if ui.button("📤 Send").clicked() {
            send_now = true;
        }
    });

    if stop_clicked {
        if let Some(h) = app.voice_playback.take() {
            h.stop();
        }
        app.playback_source = None;
    }

    if play_clicked {
        let codec = clip.codec;
        let codec_param = clip.codec_param;
        start_playback(
            app,
            &clip.payload,
            codec,
            codec_param,
            PlaybackSource::Preview,
        );
    }

    if delete_now {
        if let Some(h) = app.voice_playback.take() {
            h.stop();
        }
        app.playback_source = None;
        return VoiceCompose::Idle;
    }

    if send_now {
        if let Some(h) = app.voice_playback.take() {
            h.stop();
        }
        app.playback_source = None;
        let (ch, dest) = resolve_destination(app, nodes);
        spawn_send_voice(app, clip, ch, dest);
        return VoiceCompose::Idle;
    }

    VoiceCompose::Preview { clip }
}

fn spawn_send_voice(app: &VoicetasticApp, clip: RecordedClip, channel: u32, dest: Option<u32>) {
    // Pick chunk_size + pacing from the live modem preset. Sending at
    // MAX_BODY_SIZE (219 B) on slow presets like LongFast/LongModerate
    // pushes each frame's airtime past 1 s, so a fixed 500 ms pacing
    // overruns the firmware queue and most chunks are dropped before they
    // ever hit the air. The recommended pairing comes from VOICE_PROTOCOL.md
    // §2.1 / §4 and matches what the Android app uses.
    let preset = app
        .shared
        .lock()
        .lora
        .as_ref()
        .and_then(|l: &LoRaConfig| VoiceModemPreset::from_proto(l.modem_preset));
    let chunk_size = preset
        .map(VoiceModemPreset::recommended_chunk_size)
        .unwrap_or(MAX_BODY_SIZE);
    let pacing = preset
        .map(VoiceModemPreset::pacing)
        .unwrap_or_else(VoiceModemPreset::fallback_pacing);

    // Scale FEC parity to message size. Real LoRa broadcast links
    // routinely show 30–45 % per-chunk loss, so a fixed 8 parity shards
    // can only ever heal short messages. For longer clips we add ~50 %
    // parity (clamped to the protocol max of 128) so FEC alone closes
    // most gaps and the NACK + retransmit loop only has to mop up.
    let total_data = clip.payload.len().div_ceil(chunk_size).max(1);
    let parity_count = {
        let target = total_data.div_ceil(2).max(8);
        let cap = MAX_PARITY_PER_MESSAGE.min(255usize.saturating_sub(total_data));
        target.min(cap).min(u8::MAX as usize) as u8
    };

    let message_id = match random_message_id() {
        Ok(id) => id,
        Err(e) => {
            app.shared.lock().status_msg = Some(format!("Voice send aborted: {e}"));
            return;
        }
    };
    let cfg = BuildConfig {
        message_id,
        stream_seq: 0,
        codec: clip.codec,
        codec_param: clip.codec_param,
        chunk_size,
        parity_count,
        last_in_stream: true,
        encryption: None,
    };
    let encoded = match build_message(&clip.payload, &cfg) {
        Ok(e) => e,
        Err(e) => {
            app.shared.lock().status_msg = Some(format!("Voice build failed: {e}"));
            return;
        }
    };

    // Register frames so the inbound NACK watcher can retransmit them on
    // demand. Skipped for broadcasts (the firmware drops want_ack on
    // broadcast anyway and we have no single peer to receive NACKs from,
    // although the assembler will still relay them — keeping the entry
    // alive is cheap, so register unconditionally).
    app.outgoing_voice
        .register(cfg.message_id, &encoded, channel, dest);

    let svc = app.service.clone();
    let shared = Arc::clone(&app.shared);
    let bytes = clip.payload.clone();
    let clip_codec = clip.codec;
    let clip_codec_param = clip.codec_param;
    let duration_ms = clip.duration.as_millis() as u32;
    let to_num = dest.unwrap_or(BROADCAST_ADDR);
    let total_chunks = encoded.total_data;
    let total_bytes = clip.payload.len();
    let message_id = cfg.message_id;
    let sending_label = format!("🎙 sending voice ({total_bytes} bytes, {total_chunks} chunks)…");
    let sent_label = format!("🎙 voice message ({total_bytes} bytes, {total_chunks} chunks)");

    // Push a placeholder entry now so the chat reflects the in-flight
    // message immediately. `voice` stays `None` until the TX queue has
    // actually transmitted every frame — that's what gates the ▶ Play
    // button so users can't try to play back something that hasn't been
    // sent yet.
    shared.lock().push_chat(ChatEntry {
        text: sending_label,
        rx_time: 0,
        outgoing: true,
        channel,
        from_num: 0,
        to_num,
        voice: None,
        outgoing_voice_id: Some(message_id),
        inbound_voice_id: None,
    });

    app.rt.spawn(async move {
        // Subscribe to QueueStatus *before* sending so we don't miss the
        // confirmation for the very first frame. `mesh_packet_id` lets
        // us tick a "sent N/M" counter as each chunk leaves the radio
        // (≈ goes on air), giving the chat live progress instead of a
        // single transition from "sending …" to "sent".
        let mut qs_rx = svc.subscribe_queue_status();
        let want_ack = dest.is_some();
        let total_frames = encoded.frames.len();
        let total_data = encoded.total_data as usize;
        let mut pending_ids: std::collections::HashSet<u32> =
            std::collections::HashSet::with_capacity(total_frames);
        // Subset of `pending_ids` that map to original DATA frames
        // (indices < total_data). The user-visible "(N/M chunks)" counter
        // only ticks for these so its denominator matches the final
        // "voice message (... chunks)" label, which is also data-only.
        // FEC parity frames still flow through `pending_ids` so the
        // drain loop waits for the whole batch before flipping to
        // "voice message", but they don't inflate the counter.
        let mut pending_data_ids: std::collections::HashSet<u32> =
            std::collections::HashSet::with_capacity(total_data);
        let mut sent_on_air: usize = 0;
        let mut send_err: Option<voicetastic_core::error::Error> = None;
        for (i, frame) in encoded.frames.iter().enumerate() {
            // Drain any QS events that landed since the last enqueue so
            // the counter keeps up even if the firmware confirms faster
            // than we can push. On `Lagged` we lose precise ticks for
            // the missed events but must NOT abandon the loop — the
            // drain at the bottom + the forced N/N at the end papers
            // over the lost precision.
            loop {
                use tokio::sync::broadcast::error::TryRecvError;
                match qs_rx.try_recv() {
                    Ok(ev) => {
                        if ev.mesh_packet_id != 0
                            && pending_ids.remove(&ev.mesh_packet_id)
                            && pending_data_ids.remove(&ev.mesh_packet_id)
                        {
                            sent_on_air += 1;
                            update_sending_label(&shared, message_id, sent_on_air, total_data);
                        }
                    }
                    Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                    Err(TryRecvError::Lagged(_)) => continue,
                }
            }
            match svc
                .enqueue_voice_frame_with_id(frame.clone(), channel, dest, want_ack, pacing)
                .await
            {
                Ok(id) => {
                    pending_ids.insert(id);
                    if i < total_data {
                        pending_data_ids.insert(id);
                    }
                }
                Err(e) => {
                    send_err = Some(e);
                    break;
                }
            }
        }

        if let Some(e) = send_err {
            let mut st = shared.lock();
            if let Some(entry) = st
                .chat_log
                .iter_mut()
                .rev()
                .find(|e| e.outgoing_voice_id == Some(message_id))
            {
                entry.text = format!("🎙 voice send failed: {e}");
            }
            st.status_msg = Some(format!("Voice send failed: {e}"));
            return;
        }

        // Drain remaining QS confirmations until all enqueued packets
        // are accounted for or a short safety timeout elapses. The
        // firmware *should* emit a QS for every accepted packet but in
        // practice can batch, drop, or report with `mesh_packet_id = 0`
        // — so this loop is best-effort. On lag we keep going; on
        // timeout we exit and force the visible counter to N/N below,
        // because the for-loop completing already means every frame
        // was handed to the radio successfully.
        let drain_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !pending_data_ids.is_empty() && tokio::time::Instant::now() < drain_deadline {
            use tokio::sync::broadcast::error::RecvError;
            let remaining = drain_deadline - tokio::time::Instant::now();
            match tokio::time::timeout(remaining, qs_rx.recv()).await {
                Ok(Ok(ev)) => {
                    if ev.mesh_packet_id != 0
                        && pending_ids.remove(&ev.mesh_packet_id)
                        && pending_data_ids.remove(&ev.mesh_packet_id)
                    {
                        sent_on_air += 1;
                        update_sending_label(&shared, message_id, sent_on_air, total_data);
                    }
                }
                Ok(Err(RecvError::Lagged(_))) => continue,
                Ok(Err(RecvError::Closed)) | Err(_) => break,
            }
        }

        // Ensure the user sees a clean "N/N chunks" tick right before
        // the final "voice message (... N chunks)" label, even if some
        // QS events were lost / never matched. The for-loop completed
        // successfully which means every data frame was accepted by
        // the radio, so the count is honest.
        if sent_on_air < total_data {
            update_sending_label(&shared, message_id, total_data, total_data);
        }

        let mut st = shared.lock();
        if let Some(entry) = st
            .chat_log
            .iter_mut()
            .rev()
            .find(|e| e.outgoing_voice_id == Some(message_id))
        {
            entry.text = sent_label;
            entry.voice = Some(VoicePayload {
                codec: clip_codec,
                codec_param: clip_codec_param,
                bytes,
                duration_ms,
            });
        }
    });
}

fn update_sending_label(
    shared: &Arc<Mutex<SharedState>>,
    message_id: u32,
    sent: usize,
    total: usize,
) {
    let label = format!("🎙 sending voice ({sent}/{total} chunks)…");
    let mut st = shared.lock();
    if let Some(entry) = st
        .chat_log
        .iter_mut()
        .rev()
        .find(|e| e.outgoing_voice_id == Some(message_id))
    {
        entry.text = label;
    }
}
