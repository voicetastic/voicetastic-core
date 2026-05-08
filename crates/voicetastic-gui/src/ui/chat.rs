use std::sync::Arc;

use eframe::egui;

use crate::app::VoicetasticApp;
use crate::state::ChatEntry;

pub fn show(app: &mut VoicetasticApp, ui: &mut egui::Ui) {
    ui.heading("Text Chat");
    ui.separator();

    // Messages scroll area
    let log = app.shared.lock().chat_log.clone();
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .max_height(ui.available_height() - 40.0)
        .show(ui, |ui| {
            for entry in &log {
                let prefix = if entry.outgoing {
                    "→ You"
                } else {
                    &entry.from_id
                };
                ui.label(format!("{prefix}: {}", entry.text));
            }
        });

    // Input
    ui.separator();
    ui.horizontal(|ui| {
        ui.label("Ch:");
        ui.add(egui::DragValue::new(&mut app.chat_channel).range(0..=7));
        let resp = ui.text_edit_singleline(&mut app.chat_input);
        if (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
            || ui.button("Send").clicked()
        {
            let text = app.chat_input.clone();
            if !text.is_empty() {
                app.chat_input.clear();
                let svc = app.service.clone();
                let ch = app.chat_channel;
                let shared = Arc::clone(&app.shared);
                app.rt.spawn(async move {
                    match svc.send_text(&text, ch, None).await {
                        Ok(_id) => {
                            shared.lock().chat_log.push(ChatEntry {
                                from_id: String::new(),
                                text,
                                rx_time: 0,
                                outgoing: true,
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
