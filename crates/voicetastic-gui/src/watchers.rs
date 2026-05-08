use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Runtime;

use voicetastic_core::service::MeshService;

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
            .filter(|c| st.dirty.contains(&Section::Channel(c.index as u32)))
            .cloned()
            .collect();
        let mut next: Vec<_> = v
            .into_iter()
            .filter(|c| !st.dirty.contains(&Section::Channel(c.index as u32)))
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
                            from_id: msg.from_id.clone(),
                            text: msg.text.clone(),
                            rx_time: msg.rx_time,
                            outgoing: false,
                        });
                        c.request_repaint();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(_) => {} // lagged
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
}
