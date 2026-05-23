//! "Appearance" settings card — desktop GUI theme preferences.
//!
//! Pure client-side state stored via [`SettingsApi`]. Two knobs:
//! * **Theme mode** — `system` / `light` / `dark`. Drives egui's
//!   `ThemePreference`.
//! * **Theme contrast** — `standard` / `high`. Picks between the M3
//!   TonalSpot palette and the HighContrast variant that mirrors the
//!   firmware `meshtastic-device-ui` theme.
//!
//! Mutations flow through `app.settings.set_*`; the live application of
//! the new style happens on the matching [`SettingKey`] listener wired
//! up in [`crate::app`] (`ThemeListener`).

use eframe::egui;

use voicetastic_core::settings::{
    DEFAULT_THEME_CONTRAST, DEFAULT_THEME_MODE, SettingKey, THEME_CONTRAST_HIGH,
    THEME_CONTRAST_STANDARD, THEME_MODE_DARK, THEME_MODE_LIGHT, THEME_MODE_SYSTEM,
    ThemeContrastKind, ThemeModeKind,
};

use crate::app::VoicetasticApp;

fn warn(ctx: &str, err: impl std::fmt::Display) {
    tracing::warn!(target: "voicetastic_gui::settings", "{ctx} failed: {err}");
}

pub fn section(ui: &mut egui::Ui, app: &mut VoicetasticApp) {
    egui::CollapsingHeader::new("Appearance")
        .id_salt("appearance_settings")
        .show(ui, |ui| {
            // ---- Theme mode -------------------------------------------------
            ui.label("Theme:");
            let current_mode = app.settings.theme_mode();
            let mut next_mode = current_mode;
            let mode_label = |m: ThemeModeKind| match m {
                ThemeModeKind::System => "Follow system",
                ThemeModeKind::Light => "Light",
                ThemeModeKind::Dark => "Dark",
            };
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("theme_mode_select")
                    .selected_text(mode_label(current_mode))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut next_mode,
                            ThemeModeKind::System,
                            mode_label(ThemeModeKind::System),
                        );
                        ui.selectable_value(
                            &mut next_mode,
                            ThemeModeKind::Light,
                            mode_label(ThemeModeKind::Light),
                        );
                        ui.selectable_value(
                            &mut next_mode,
                            ThemeModeKind::Dark,
                            mode_label(ThemeModeKind::Dark),
                        );
                    });
                if ui.small_button("Reset").clicked()
                    && let Err(e) = app.settings.reset(SettingKey::ThemeMode)
                {
                    warn("reset theme_mode", e);
                }
            });
            if next_mode != current_mode
                && let Err(e) = app.settings.set_theme_mode(next_mode)
            {
                warn("set theme_mode", e);
            }
            ui.weak(format!(
                "Default: {} ({THEME_MODE_SYSTEM}/{THEME_MODE_LIGHT}/{THEME_MODE_DARK}). \
                 `System` follows the host preference; the other two pin the scheme.",
                DEFAULT_THEME_MODE
            ));

            // ---- Theme contrast --------------------------------------------
            ui.add_space(6.0);
            ui.label("Contrast:");
            let current_contrast = app.settings.theme_contrast();
            let mut next_contrast = current_contrast;
            let contrast_label = |c: ThemeContrastKind| match c {
                ThemeContrastKind::Standard => "Standard (warm peach)",
                ThemeContrastKind::High => "High contrast (device-ui parity / a11y)",
            };
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("theme_contrast_select")
                    .selected_text(contrast_label(current_contrast))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut next_contrast,
                            ThemeContrastKind::Standard,
                            contrast_label(ThemeContrastKind::Standard),
                        );
                        ui.selectable_value(
                            &mut next_contrast,
                            ThemeContrastKind::High,
                            contrast_label(ThemeContrastKind::High),
                        );
                    });
                if ui.small_button("Reset").clicked()
                    && let Err(e) = app.settings.reset(SettingKey::ThemeContrast)
                {
                    warn("reset theme_contrast", e);
                }
            });
            if next_contrast != current_contrast
                && let Err(e) = app.settings.set_theme_contrast(next_contrast)
            {
                warn("set theme_contrast", e);
            }
            ui.weak(format!(
                "Default: {} ({THEME_CONTRAST_STANDARD}/{THEME_CONTRAST_HIGH}). \
                 `High` mirrors the meshtastic-device-ui firmware theme — pure-black-on-near-white \
                 (and its inverse) for small displays or accessibility needs.",
                DEFAULT_THEME_CONTRAST
            ));
        });
}
