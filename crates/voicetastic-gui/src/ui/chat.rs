use std::collections::BTreeSet;
use std::sync::Arc;

use eframe::egui;

use voicetastic_core::ports::BROADCAST_ADDR;
use voicetastic_core::proto::{
    Channel, NodeInfo, channel::Role, config::LoRaConfig, config::lo_ra_config::ModemPreset,
};

use crate::app::VoicetasticApp;
use crate::state::ChatEntry;

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

    // Messages for the active thread.
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .max_height(ui.available_height() - 40.0)
        .show(ui, |ui| {
            let mut any = false;
            for entry in log.iter() {
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
                ui.label(format!("{prefix}: {}", entry.text));
            }
            if !any {
                ui.weak("(no messages in this conversation yet)");
            }
        });

    // Input row.
    ui.separator();
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
                            shared.lock().chat_log.push(ChatEntry {
                                text,
                                rx_time: 0,
                                outgoing: true,
                                channel: ch,
                                from_num: 0,
                                to_num,
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
