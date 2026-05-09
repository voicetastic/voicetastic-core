//! Bluetooth section.

use eframe::egui;

use voicetastic_core::proto::config::{self, bluetooth_config};

use super::widgets::{enum_combo, u32_field};
use super::{CardMeta, Ctx, card, write_config_fut};
use crate::state::Section;

pub(super) fn section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    card(
        ui,
        ctx,
        CardMeta {
            title: "🔵 Bluetooth",
            id: "bluetooth",
            section: Section::Bluetooth,
            name: "Bluetooth Config",
        },
        |s| s.bluetooth,
        |s, v| s.bluetooth = Some(v),
        |ui, c| {
            let mut ch = false;
            ch |= ui.checkbox(&mut c.enabled, "Enabled").changed();
            ch |= enum_combo::<bluetooth_config::PairingMode>(
                ui,
                "Pairing mode",
                &mut c.mode,
                "bt_mode",
            );
            ch |= u32_field(ui, "Fixed PIN", &mut c.fixed_pin);
            ch
        },
        |svc, c| write_config_fut(svc, config::PayloadVariant::Bluetooth(c)),
    );
}
