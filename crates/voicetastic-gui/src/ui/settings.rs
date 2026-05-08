use eframe::egui;

use crate::app::VoicetasticApp;

pub fn show(app: &mut VoicetasticApp, ui: &mut egui::Ui) {
    ui.heading("Settings");
    ui.separator();

    let info = app.shared.lock().my_info.clone();
    if let Some(info) = info {
        ui.label(format!("My node num: {}", info.my_node_num));
    } else {
        ui.label("Not connected");
    }

    ui.separator();
    ui.label("Voice codec: AMR-NB (save/load .amr files only)");
    ui.label("No live microphone support in this version.");
}
