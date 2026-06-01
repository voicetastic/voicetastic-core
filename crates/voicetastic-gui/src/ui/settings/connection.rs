//! Top-of-tab connection card, status banner, and danger-zone actions.

use std::sync::Arc;

use eframe::egui;

use voicetastic_core::meshtastic::service::ConnectionState;

use super::{Ctx, run_status};

pub(super) fn connection_card(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    let (state, my_num, fw, dirty_count) = {
        let s = ctx.shared.lock();
        (
            s.conn_state,
            s.my_info.as_ref().map(|i| i.my_node_num),
            s.metadata.as_ref().map(|m| m.firmware_version.clone()),
            s.dirty.len(),
        )
    };

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            let label = match state {
                ConnectionState::Disconnected => "⚪ Disconnected",
                ConnectionState::Connecting => "🟡 Connecting…",
                ConnectionState::Connected => "🟢 Connected",
                ConnectionState::Configuring => "🟡 Configuring…",
                ConnectionState::Ready => "🟢 Ready",
            };
            ui.heading(label);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⟳ Refresh config").clicked() {
                    let svc = ctx.svc.clone();
                    let shared = Arc::clone(ctx.shared);
                    ctx.rt.spawn(async move {
                        shared.lock().config_status = Some("Refresh requested…".into());
                        if let Err(e) = svc.refresh_config().await {
                            shared.lock().config_status = Some(format!("Refresh failed: {e}"));
                        }
                    });
                }
            });
        });
        if let Some(num) = my_num {
            ui.label(format!("Node: 0x{num:08x} ({num})"));
        }
        if let Some(v) = fw {
            ui.label(format!("Firmware: {v}"));
        }

        // Warn the user if they have unsaved edits while the device went
        // away — clicking Apply now is a no-op, the dirty flags will be
        // cleared by the next config burst, and any local changes lost.
        if dirty_count > 0 && !matches!(state, ConnectionState::Ready) {
            ui.colored_label(
                ui.style().visuals.warn_fg_color,
                format!(
                    "⚠ {dirty_count} unsaved edit(s); reconnect to Ready before Apply, \
                     or they will be discarded by the next config push"
                ),
            );
        }
    });
}

pub(super) fn status_card(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    let status = ctx.shared.lock().config_status.clone();
    if let Some(msg) = status {
        ui.horizontal(|ui| {
            ui.label(msg);
            if ui.small_button("Dismiss").clicked() {
                ctx.shared.lock().config_status = None;
            }
        });
    }
}

pub(super) fn actions_section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    egui::CollapsingHeader::new("⚠ Device actions")
        .id_salt("actions")
        .show(ui, |ui| {
            if ui.button("Reboot device (5s)").clicked() {
                run_status(ctx, "Reboot", |svc| {
                    Box::pin(async move { svc.reboot(5).await.map(|_| ()) })
                });
            }
            ui.add_space(4.0);
            if ui.button("Reset NodeDB").clicked() {
                run_status(ctx, "Reset NodeDB", |svc| {
                    Box::pin(async move { svc.reset_nodedb_and_refresh().await })
                });
            }
            ui.add_space(4.0);
            let red = ui.style().visuals.error_fg_color;
            let btn = egui::Button::new(egui::RichText::new("Factory reset").color(red));
            if ui.add(btn).clicked() {
                run_status(ctx, "Factory reset", |svc| {
                    Box::pin(async move { svc.factory_reset().await.map(|_| ()) })
                });
            }
        });
}
