//! "Voice messages" settings card — purely client-side preferences (max
//! recording duration). Distinct from the device-config sections because
//! none of this is shipped over the air; it lives in `AppSettings` and is
//! persisted to the local config file.

use eframe::egui;

use voicetastic_core::settings::{
    DEFAULT_REASSEMBLY_TIMEOUT_SECS, DEFAULT_VOICE_MAX_SECS, REASSEMBLY_TIMEOUT_LOWER_SECS,
    REASSEMBLY_TIMEOUT_UPPER_SECS, VOICE_MAX_SECS_UPPER,
};

use crate::app::VoicetasticApp;
use crate::audio;

pub fn section(ui: &mut egui::Ui, app: &mut VoicetasticApp) {
    egui::CollapsingHeader::new("Voice messages")
        .id_salt("voice_settings")
        .show(ui, |ui| {
            let mut secs = app.app_settings.voice_max_secs();
            ui.horizontal(|ui| {
                ui.label("Max recording duration:");
                if ui
                    .add(
                        egui::Slider::new(&mut secs, 1..=VOICE_MAX_SECS_UPPER)
                            .suffix(" s")
                            .clamping(egui::SliderClamping::Always),
                    )
                    .changed()
                {
                    app.app_settings.max_voice_duration_secs = Some(secs);
                    app.save_settings();
                }
                if ui.small_button("Reset").clicked() {
                    app.app_settings.max_voice_duration_secs = None;
                    app.save_settings();
                }
            });
            ui.weak(format!(
                "Default: {DEFAULT_VOICE_MAX_SECS} s. Recording stops automatically when the cap is reached."
            ));

            ui.add_space(6.0);
            let mut timeout = app.app_settings.reassembly_timeout_secs();
            ui.horizontal(|ui| {
                ui.label("Reassembly timeout:");
                if ui
                    .add(
                        egui::Slider::new(
                            &mut timeout,
                            REASSEMBLY_TIMEOUT_LOWER_SECS..=REASSEMBLY_TIMEOUT_UPPER_SECS,
                        )
                        .suffix(" s")
                        .logarithmic(true)
                        .clamping(egui::SliderClamping::Always),
                    )
                    .changed()
                {
                    app.app_settings.reassembly_timeout_secs = Some(timeout);
                    app.save_settings();
                    app.apply_voice_settings();
                }
                if ui.small_button("Reset").clicked() {
                    app.app_settings.reassembly_timeout_secs = None;
                    app.save_settings();
                    app.apply_voice_settings();
                }
            });
            ui.weak(format!(
                "Default: {DEFAULT_REASSEMBLY_TIMEOUT_SECS} s. How long the receiver waits for missing \
                 chunks of an in-flight voice message before emitting a partial. Applies immediately."
            ));
            if !audio::is_available() {
                ui.add_space(4.0);
                ui.colored_label(
                    egui::Color32::from_rgb(200, 140, 60),
                    "Audio support is disabled in this build. Rebuild the GUI with \
                     `--features audio` to enable mic capture and playback.",
                );
            }
        });
}
