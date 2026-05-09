//! Network section.

use eframe::egui;

use voicetastic_core::proto::config::{self, network_config};

use super::widgets::{enum_combo, secret_field, str_field};
use super::{CardMeta, Ctx, card, write_config_fut};
use crate::state::Section;

pub(super) fn section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    card(
        ui,
        ctx,
        CardMeta {
            title: "🌐 Network",
            id: "network",
            section: Section::Network,
            name: "Network Config",
        },
        |s| s.network.clone(),
        |s, v| s.network = Some(v),
        |ui, c| {
            let mut ch = false;
            ch |= ui.checkbox(&mut c.wifi_enabled, "WiFi enabled").changed();
            ch |= str_field(ui, "WiFi SSID", &mut c.wifi_ssid);
            ch |= secret_field(ui, "WiFi PSK", &mut c.wifi_psk, "net_psk");
            ch |= ui
                .checkbox(&mut c.eth_enabled, "Ethernet enabled")
                .changed();
            ch |= enum_combo::<network_config::AddressMode>(
                ui,
                "Address mode",
                &mut c.address_mode,
                "net_addr",
            );
            ch |= str_field(ui, "NTP server", &mut c.ntp_server);
            ch |= str_field(ui, "Rsyslog server", &mut c.rsyslog_server);
            ch
        },
        |svc, c| write_config_fut(svc, config::PayloadVariant::Network(c)),
    );
}
