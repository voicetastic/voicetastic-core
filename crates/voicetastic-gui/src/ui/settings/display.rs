//! Display section.

use eframe::egui;

use voicetastic_core::proto::config::{self, display_config};

use super::widgets::{enum_combo, u32_field};
use super::{CardMeta, Ctx, card, write_config_fut};
use crate::state::Section;

pub(super) fn section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    card(
        ui,
        ctx,
        CardMeta {
            title: "🖥 Display",
            id: "display",
            section: Section::Display,
            name: "Display Config",
        },
        |s| s.display,
        |s, v| s.display = Some(v),
        |ui, c| {
            let mut ch = false;
            ch |= u32_field(ui, "Screen on (s)", &mut c.screen_on_secs);
            ch |= u32_field(ui, "Auto carousel (s)", &mut c.auto_screen_carousel_secs);
            ch |=
                enum_combo::<display_config::DisplayUnits>(ui, "Units", &mut c.units, "dsp_units");
            ch |= enum_combo::<display_config::OledType>(ui, "OLED type", &mut c.oled, "dsp_oled");
            ch |= enum_combo::<display_config::DisplayMode>(
                ui,
                "Display mode",
                &mut c.displaymode,
                "dsp_mode",
            );
            ch |= ui.checkbox(&mut c.flip_screen, "Flip screen").changed();
            ch |= ui.checkbox(&mut c.heading_bold, "Heading bold").changed();
            ch |= ui
                .checkbox(&mut c.wake_on_tap_or_motion, "Wake on tap/motion")
                .changed();
            ch |= ui.checkbox(&mut c.use_12h_clock, "12h clock").changed();
            ch |= ui
                .checkbox(&mut c.use_long_node_name, "Long node names")
                .changed();
            ch
        },
        |svc, c| write_config_fut(svc, config::PayloadVariant::Display(c)),
    );
}
