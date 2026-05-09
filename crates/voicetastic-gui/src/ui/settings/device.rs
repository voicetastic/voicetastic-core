//! Device section.

use eframe::egui;

use voicetastic_core::proto::config::{self, device_config};

use super::widgets::{enum_combo, u32_field};
use super::{CardMeta, Ctx, card, write_config_fut};
use crate::state::Section;

pub(super) fn section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    card(
        ui,
        ctx,
        CardMeta {
            title: "📱 Device",
            id: "device",
            section: Section::Device,
            name: "Device Config",
        },
        |s| s.device.clone(),
        |s, v| s.device = Some(v),
        |ui, c| {
            let mut ch = false;
            ch |= enum_combo::<device_config::Role>(ui, "Role", &mut c.role, "dev_role");
            ch |= enum_combo::<device_config::RebroadcastMode>(
                ui,
                "Rebroadcast mode",
                &mut c.rebroadcast_mode,
                "dev_rb",
            );
            ch |= u32_field(
                ui,
                "NodeInfo broadcast (s)",
                &mut c.node_info_broadcast_secs,
            );
            ch |= ui
                .checkbox(&mut c.double_tap_as_button_press, "Double tap as button")
                .changed();
            ch |= ui
                .checkbox(&mut c.disable_triple_click, "Disable triple click")
                .changed();
            ch |= u32_field(ui, "Button GPIO", &mut c.button_gpio);
            ch |= u32_field(ui, "Buzzer GPIO", &mut c.buzzer_gpio);
            ch
        },
        |svc, c| write_config_fut(svc, config::PayloadVariant::Device(c)),
    );
}
