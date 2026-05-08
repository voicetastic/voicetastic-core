use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Runtime;

use voicetastic_core::service::MeshService;

use crate::state::{ChatEntry, SharedState};

pub fn spawn_watchers(
    rt: &Runtime,
    svc: &MeshService,
    shared: Arc<Mutex<SharedState>>,
    ctx: egui::Context,
) {
    // Connection state
    {
        let mut rx = svc.watch_state();
        let s = Arc::clone(&shared);
        let c = ctx.clone();
        rt.spawn(async move {
            while rx.changed().await.is_ok() {
                s.lock().conn_state = *rx.borrow_and_update();
                c.request_repaint();
            }
        });
    }
    // My info
    {
        let mut rx = svc.watch_my_info();
        let s = Arc::clone(&shared);
        let c = ctx.clone();
        rt.spawn(async move {
            while rx.changed().await.is_ok() {
                s.lock().my_info = rx.borrow_and_update().clone();
                c.request_repaint();
            }
        });
    }
    // Nodes
    {
        let mut rx = svc.watch_nodes();
        let s = Arc::clone(&shared);
        let c = ctx.clone();
        rt.spawn(async move {
            while rx.changed().await.is_ok() {
                s.lock().nodes = rx.borrow_and_update().clone();
                c.request_repaint();
            }
        });
    }
    // Incoming text
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
}
