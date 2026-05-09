use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Runtime;

use voicetastic_core::ids::node_num_to_id;
use voicetastic_core::ports::{BROADCAST_ADDR, PRIVATE_APP};
use voicetastic_core::service::MeshService;
use voicetastic_core::voice::{
    AssemblerConfig, AssemblyEvent, VoiceAssembler, VoiceDestination, VoiceMessage, detect_version,
};

use crate::state::{ChatEntry, Section, SharedState};

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
                        s.lock().chat_log.push(ChatEntry {
                            text: msg.text.clone(),
                            rx_time: msg.rx_time,
                            outgoing: false,
                            channel: msg.channel,
                            from_num: msg.from,
                            to_num: msg.to,
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
    // when a message completes (or partially completes on timeout).
    {
        let mut rx = svc.subscribe_data();
        let s = Arc::clone(&shared);
        let c = ctx.clone();
        rt.spawn(async move {
            let assembler = VoiceAssembler::new(AssemblerConfig::default());
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let out = assembler.tick();
                        for completed in out.finalized {
                            push_voice_entry(&s, &c, &completed);
                        }
                    }
                    msg = rx.recv() => match msg {
                        Ok(d) => {
                            if d.portnum != PRIVATE_APP as i32 { continue; }
                            if detect_version(&d.payload) != Some(0x01) { continue; }
                            let from_id = node_num_to_id(d.from);
                            let to = if d.to == BROADCAST_ADDR {
                                VoiceDestination::Broadcast
                            } else {
                                VoiceDestination::Node(d.to)
                            };
                            if let AssemblyEvent::Complete(m) =
                                assembler.accept(&from_id, to, d.channel, &d.payload)
                            {
                                push_voice_entry(&s, &c, &m);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "voice broadcast lagged");
                        }
                    },
                }
            }
        });
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
    s.lock().chat_log.push(ChatEntry {
        text: label,
        rx_time: 0,
        outgoing: false,
        channel: msg.channel,
        from_num: 0,
        to_num: 0,
    });
    c.request_repaint();
}
