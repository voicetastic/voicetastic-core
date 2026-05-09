//! Channels — list of independently editable rows. Doesn't fit `card<T>` so
//! we open it manually but reuse the same field widgets and `spawn_apply`.

use eframe::egui;

use voicetastic_core::proto::channel;

use super::widgets::{bytes_to_hex, hex_to_bytes, secret_field, str_field};
use super::{Ctx, spawn_apply};
use crate::state::Section;

pub(super) fn section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    egui::CollapsingHeader::new("💬 Channels")
        .id_salt("channels")
        .show(ui, |ui| {
            let channels = ctx.shared.lock().channels.clone();
            if channels.is_empty() {
                ui.label("(no channels received yet)");
                return;
            }
            for ch in channels {
                render_channel_row(ui, ctx, ch);
            }
        });
}

fn render_channel_row(ui: &mut egui::Ui, ctx: &Ctx<'_>, ch: voicetastic_core::proto::Channel) {
    let idx = ch.index;
    ui.group(|ui| {
        let role_name = channel::Role::try_from(ch.role)
            .map(|r| r.as_str_name().to_string())
            .unwrap_or_else(|_| format!("#{}", ch.role));
        ui.label(format!("Channel {idx} ({role_name})"));

        let mut next = ch.clone();
        let mut settings = next.settings.unwrap_or_default();
        // Persist the PSK hex buffer across frames so the user can type
        // intermediate odd-length / partial states without the display
        // snapping back to the on-device value.
        let psk_id = ui.id().with(("ch_psk_buf", idx));
        let stored_buf: Option<String> = ui.data_mut(|d| d.get_temp(psk_id));
        let on_device_hex = bytes_to_hex(&settings.psk);
        let mut psk_hex = stored_buf.unwrap_or_else(|| on_device_hex.clone());
        // If the device just pushed a different PSK and we don't have an
        // in-flight edit (i.e. our buffer matches some valid bytes), reseed.
        if hex_to_bytes(&psk_hex)
            .map(|b| b == settings.psk)
            .unwrap_or(false)
            && psk_hex.to_ascii_lowercase() != on_device_hex
        {
            psk_hex = on_device_hex.clone();
        }

        let mut changed = false;
        changed |= str_field(ui, "Name", &mut settings.name);
        let psk_changed = secret_field(ui, "PSK (hex)", &mut psk_hex, &format!("ch_psk_{idx}"));
        let psk_bytes = hex_to_bytes(&psk_hex);
        if psk_bytes.is_none() {
            ui.colored_label(
                ui.style().visuals.error_fg_color,
                "PSK must be valid hex (even number of hex digits)",
            );
        }
        if let Some(b) = psk_bytes.as_ref()
            && b != &settings.psk
        {
            settings.psk = b.clone();
            changed = true;
        }
        // Persist the buffer for the next frame regardless of validity.
        ui.data_mut(|d| d.insert_temp(psk_id, psk_hex.clone()));
        // Typing invalid hex still counts as a pending edit so watchers
        // don't clobber it, but Apply should refuse to send.
        let pending_edit = psk_changed && psk_bytes.is_none();

        changed |= ui
            .checkbox(&mut settings.uplink_enabled, "Uplink enabled")
            .changed();
        changed |= ui
            .checkbox(&mut settings.downlink_enabled, "Downlink enabled")
            .changed();
        next.settings = Some(settings);

        let section = Section::Channel(idx);
        if changed || pending_edit {
            let mut s = ctx.shared.lock();
            s.dirty.insert(section);
            if changed && let Some(slot) = s.channels.iter_mut().find(|c| c.index == idx) {
                *slot = next.clone();
            }
        }
        let apply = ui.add_enabled(
            psk_bytes.is_some(),
            egui::Button::new(format!("Apply Channel {idx}")),
        );
        if apply.clicked() {
            spawn_apply(ctx, section, &format!("Channel {idx}"), next, |svc, c| {
                Box::pin(async move { svc.write_channel(c).await.map(|_| ()) })
            });
        }
    });
}
