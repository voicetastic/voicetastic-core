//! Power section.

use eframe::egui;

use voicetastic_core::proto::config;

use super::widgets::{f32_field, u32_field};
use super::{CardMeta, Ctx, card, write_config_fut};
use crate::state::Section;

pub(super) fn section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    card(
        ui,
        ctx,
        CardMeta {
            title: "🔋 Power",
            id: "power",
            section: Section::Power,
            name: "Power Config",
        },
        |s| s.power,
        |s, v| s.power = Some(v),
        |ui, c| {
            let mut ch = false;
            ch |= ui
                .checkbox(&mut c.is_power_saving, "Power saving")
                .changed();
            ch |= u32_field(
                ui,
                "Shutdown after (s, on battery)",
                &mut c.on_battery_shutdown_after_secs,
            );
            ch |= u32_field(ui, "Wait Bluetooth (s)", &mut c.wait_bluetooth_secs);
            ch |= u32_field(ui, "SDS secs", &mut c.sds_secs);
            ch |= u32_field(ui, "LS secs", &mut c.ls_secs);
            ch |= u32_field(ui, "Min wake secs", &mut c.min_wake_secs);
            ch |= f32_field(
                ui,
                "ADC multiplier override",
                &mut c.adc_multiplier_override,
            );
            ch
        },
        |svc, c| write_config_fut(svc, config::PayloadVariant::Power(c)),
    );
}
