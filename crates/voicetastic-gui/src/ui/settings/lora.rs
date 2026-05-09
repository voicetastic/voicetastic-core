//! LoRa / Radio section.

use eframe::egui;

use voicetastic_core::proto::config::{self, lo_ra_config};

use super::widgets::{enum_combo, f32_field, i32_field, u32_field};
use super::{CardMeta, Ctx, card, write_config_fut};
use crate::state::Section;

pub(super) fn section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    card(
        ui,
        ctx,
        CardMeta {
            title: "📡 LoRa / Radio",
            id: "lora",
            section: Section::Lora,
            name: "LoRa Config",
        },
        |s| s.lora.clone(),
        |s, v| s.lora = Some(v),
        |ui, c| {
            let mut ch = false;
            ch |= enum_combo::<lo_ra_config::RegionCode>(ui, "Region", &mut c.region, "lora_rgn");
            ch |= ui.checkbox(&mut c.use_preset, "Use preset").changed();
            if c.use_preset {
                ch |= enum_combo::<lo_ra_config::ModemPreset>(
                    ui,
                    "Modem preset",
                    &mut c.modem_preset,
                    "lora_preset",
                );
            } else {
                ch |= u32_field(ui, "Bandwidth (kHz)", &mut c.bandwidth);
                ch |= u32_field(ui, "Spread factor", &mut c.spread_factor);
                ch |= u32_field(ui, "Coding rate", &mut c.coding_rate);
            }
            ch |= u32_field(ui, "Hop limit (1-7)", &mut c.hop_limit);
            ch |= i32_field(ui, "Tx power (dBm)", &mut c.tx_power);
            ch |= ui.checkbox(&mut c.tx_enabled, "Tx enabled").changed();
            ch |= ui
                .checkbox(&mut c.override_duty_cycle, "Override duty cycle")
                .changed();
            ch |= ui
                .checkbox(&mut c.sx126x_rx_boosted_gain, "SX126x Rx boosted gain")
                .changed();
            ch |= ui.checkbox(&mut c.ignore_mqtt, "Ignore MQTT").changed();
            ch |= u32_field(ui, "Channel num", &mut c.channel_num);
            ch |= f32_field(ui, "Frequency offset (MHz)", &mut c.frequency_offset);
            ch |= f32_field(ui, "Override frequency (MHz)", &mut c.override_frequency);
            ch
        },
        |svc, c| write_config_fut(svc, config::PayloadVariant::Lora(c)),
    );
}
