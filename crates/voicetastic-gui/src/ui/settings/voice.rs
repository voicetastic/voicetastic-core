//! "Voice messages" settings card — purely client-side preferences (max
//! recording duration). Distinct from the device-config sections because
//! none of this is shipped over the air; it lives in `AppSettings` and is
//! persisted to the local config file.

use eframe::egui;

use voicetastic_core::settings::{
    CODEC2_MODE_1200, CODEC2_MODE_1300, CODEC2_MODE_1400, CODEC2_MODE_1600, CODEC2_MODE_2400,
    CODEC2_MODE_3200, DEFAULT_REASSEMBLY_TIMEOUT_SECS, DEFAULT_VOICE_CODEC, DEFAULT_VOICE_MAX_SECS,
    REASSEMBLY_TIMEOUT_LOWER_SECS, REASSEMBLY_TIMEOUT_UPPER_SECS, VOICE_CODEC_CODEC2,
    VOICE_CODEC_OPUS, VOICE_MAX_SECS_UPPER,
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

            ui.add_space(6.0);
            ui.label("Outgoing codec:");
            let current = app.app_settings.voice_codec().to_string();
            let mut next = current.clone();
            let label = match current.as_str() {
                VOICE_CODEC_OPUS => "Opus (12 kbps wideband)",
                _ => "Codec2 (1.2–3.2 kbps narrowband)",
            };
            egui::ComboBox::from_id_salt("voice_codec_select")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut next,
                        VOICE_CODEC_CODEC2.to_string(),
                        "Codec2 (1.2–3.2 kbps narrowband)",
                    );
                    ui.selectable_value(
                        &mut next,
                        VOICE_CODEC_OPUS.to_string(),
                        "Opus (12 kbps wideband)",
                    );
                });
            if next != current {
                app.app_settings.voice_codec = Some(next);
                app.save_settings();
            }

            if app.app_settings.voice_codec() == VOICE_CODEC_CODEC2 {
                ui.add_space(4.0);
                ui.label("Codec2 bitrate:");
                let mut mode = app.app_settings.voice_codec2_mode();
                let mode_label = |m: u8| match m {
                    CODEC2_MODE_3200 => "3200 bps (best quality)",
                    CODEC2_MODE_2400 => "2400 bps",
                    CODEC2_MODE_1600 => "1600 bps",
                    CODEC2_MODE_1400 => "1400 bps",
                    CODEC2_MODE_1300 => "1300 bps",
                    CODEC2_MODE_1200 => "1200 bps (lowest, default)",
                    _ => "unknown",
                };
                let prev = mode;
                egui::ComboBox::from_id_salt("voice_codec2_mode_select")
                    .selected_text(mode_label(mode))
                    .show_ui(ui, |ui| {
                        for m in [
                            CODEC2_MODE_1200,
                            CODEC2_MODE_1300,
                            CODEC2_MODE_1400,
                            CODEC2_MODE_1600,
                            CODEC2_MODE_2400,
                            CODEC2_MODE_3200,
                        ] {
                            ui.selectable_value(&mut mode, m, mode_label(m));
                        }
                    });
                if mode != prev {
                    app.app_settings.voice_codec2_mode = Some(mode);
                    app.save_settings();
                }
            }
            ui.weak(format!(
                "Default codec: {DEFAULT_VOICE_CODEC}. Codec2 at 1200 bps fits a 30 s clip in \
                 ~4.5 kB — recommended for slow LoRa presets. Received messages are always \
                 decoded with the codec advertised in their header."
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
