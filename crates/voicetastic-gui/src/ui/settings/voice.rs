//! "Voice messages" settings card -- purely client-side preferences (max
//! recording duration). Distinct from the device-config sections because
//! none of this is shipped over the air; it lives in the centralised
//! [`SettingsApi`] (persisted as TOML under `$XDG_CONFIG_HOME`).
//!
//! Widgets in this module are display-only: every mutation flows through
//! `app.settings.set_*` so any other front-end (CLI, Android) sharing
//! the same config file sees identical effects.

use eframe::egui;

use voicetastic_core::settings::{
    AMRNB_MODE_475, AMRNB_MODE_515, AMRNB_MODE_590, AMRNB_MODE_670, AMRNB_MODE_740, AMRNB_MODE_795,
    AMRNB_MODE_1020, AMRNB_MODE_1220, CODEC2_MODE_1200, CODEC2_MODE_1300, CODEC2_MODE_1400,
    CODEC2_MODE_1600, CODEC2_MODE_2400, CODEC2_MODE_3200, DEFAULT_REASSEMBLY_TIMEOUT_SECS,
    DEFAULT_VOICE_CODEC, DEFAULT_VOICE_MAX_SECS, REASSEMBLY_TIMEOUT_LOWER_SECS,
    REASSEMBLY_TIMEOUT_UPPER_SECS, SettingKey, VOICE_MAX_SECS_UPPER, VoiceCodecKind,
};

use crate::app::VoicetasticApp;
use crate::audio;

fn warn(ctx: &str, err: impl std::fmt::Display) {
    tracing::warn!(target: "voicetastic_gui::settings", "{ctx} failed: {err}");
}

pub fn section(ui: &mut egui::Ui, app: &mut VoicetasticApp) {
    egui::CollapsingHeader::new("Voice messages")
        .id_salt("voice_settings")
        .show(ui, |ui| {
            let mut secs = app.settings.voice_max_secs();
            ui.horizontal(|ui| {
                ui.label("Max recording duration:");
                if ui
                    .add(
                        egui::Slider::new(&mut secs, 1..=VOICE_MAX_SECS_UPPER)
                            .suffix(" s")
                            .clamping(egui::SliderClamping::Always),
                    )
                    .changed()
                    && let Err(e) = app.settings.set_voice_max_secs(secs)
                {
                    warn("set voice_max_secs", e);
                }
                if ui.small_button("Reset").clicked()
                    && let Err(e) = app.settings.reset(SettingKey::VoiceMaxDurationSecs)
                {
                    warn("reset voice_max_secs", e);
                }
            });
            ui.weak(format!(
                "Default: {DEFAULT_VOICE_MAX_SECS} s. Recording stops automatically when the cap is reached."
            ));

            ui.add_space(6.0);
            let mut timeout = app.settings.reassembly_timeout_secs();
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
                    && let Err(e) = app.settings.set_reassembly_timeout_secs(timeout)
                {
                    warn("set reassembly_timeout_secs", e);
                }
                if ui.small_button("Reset").clicked()
                    && let Err(e) = app.settings.reset(SettingKey::VoiceReassemblyTimeoutSecs)
                {
                    warn("reset reassembly_timeout_secs", e);
                }
            });
            ui.weak(format!(
                "Default: {DEFAULT_REASSEMBLY_TIMEOUT_SECS} s. How long the receiver waits for missing \
                 chunks of an in-flight voice message before emitting a partial. Applies immediately."
            ));

            ui.add_space(6.0);
            ui.label("Outgoing codec:");
            let current = app.settings.voice_codec();
            let mut next = current;
            let label = |k: VoiceCodecKind| match k {
                VoiceCodecKind::Opus => "Opus (12 kbps wideband)",
                VoiceCodecKind::Codec2 => "Codec2 (1.2-3.2 kbps narrowband)",
                VoiceCodecKind::AmrNb => "AMR-NB (4.75-12.2 kbps narrowband, default)",
            };
            egui::ComboBox::from_id_salt("voice_codec_select")
                .selected_text(label(current))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut next, VoiceCodecKind::AmrNb, label(VoiceCodecKind::AmrNb));
                    ui.selectable_value(&mut next, VoiceCodecKind::Codec2, label(VoiceCodecKind::Codec2));
                    ui.selectable_value(&mut next, VoiceCodecKind::Opus, label(VoiceCodecKind::Opus));
                });
            if next != current
                && let Err(e) = app.settings.set_voice_codec(next)
            {
                warn("set voice_codec", e);
            }

            if app.settings.voice_codec() == VoiceCodecKind::Codec2 {
                ui.add_space(4.0);
                ui.label("Codec2 bitrate:");
                let mut mode = app.settings.voice_codec2_mode();
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
                if mode != prev
                    && let Err(e) = app.settings.set_voice_codec2_mode(mode)
                {
                    warn("set voice_codec2_mode", e);
                }
            }

            if app.settings.voice_codec() == VoiceCodecKind::AmrNb {
                ui.add_space(4.0);
                ui.label("AMR-NB bitrate:");
                let mut mode = app.settings.voice_amrnb_mode();
                let mode_label = |m: u8| match m {
                    AMRNB_MODE_475 => "4.75 kbps (lowest)",
                    AMRNB_MODE_515 => "5.15 kbps",
                    AMRNB_MODE_590 => "5.90 kbps",
                    AMRNB_MODE_670 => "6.70 kbps",
                    AMRNB_MODE_740 => "7.40 kbps",
                    AMRNB_MODE_795 => "7.95 kbps",
                    AMRNB_MODE_1020 => "10.20 kbps",
                    AMRNB_MODE_1220 => "12.20 kbps (best, default)",
                    _ => "unknown",
                };
                let prev = mode;
                egui::ComboBox::from_id_salt("voice_amrnb_mode_select")
                    .selected_text(mode_label(mode))
                    .show_ui(ui, |ui| {
                        for m in [
                            AMRNB_MODE_475,
                            AMRNB_MODE_515,
                            AMRNB_MODE_590,
                            AMRNB_MODE_670,
                            AMRNB_MODE_740,
                            AMRNB_MODE_795,
                            AMRNB_MODE_1020,
                            AMRNB_MODE_1220,
                        ] {
                            ui.selectable_value(&mut mode, m, mode_label(m));
                        }
                    });
                if mode != prev
                    && let Err(e) = app.settings.set_voice_amrnb_mode(mode)
                {
                    warn("set voice_amrnb_mode", e);
                }
            }
            ui.weak(format!(
                "Default codec: {DEFAULT_VOICE_CODEC}. Codec2 at 1200 bps fits a 30 s clip in \
                 ~4.5 kB -- recommended for slow LoRa presets. Received messages are always \
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
