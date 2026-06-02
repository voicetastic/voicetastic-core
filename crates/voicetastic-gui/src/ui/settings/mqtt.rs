//! MQTT module-config section.

use eframe::egui;

use voicetastic_core::meshtastic::service::MeshtasticService;
use voicetastic_core::proto::module_config::{self, MqttConfig};

use super::widgets::{secret_field, str_field};
use super::{ApplyFut, CardMeta, Ctx, card};
use crate::state::Section;

pub(super) fn section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    card(
        ui,
        ctx,
        CardMeta {
            title: "☁ MQTT gateway",
            id: "mqtt",
            section: Section::Mqtt,
            name: "MQTT Module",
        },
        |s| s.mqtt.clone(),
        |s, v| s.mqtt = Some(v),
        |ui, c: &mut MqttConfig| {
            let mut ch = false;
            ch |= ui.checkbox(&mut c.enabled, "Enabled").changed();
            ch |= str_field(ui, "Server address", &mut c.address);
            ch |= str_field(ui, "Username", &mut c.username);
            ch |= secret_field(ui, "Password", &mut c.password, "mqtt_pw");
            ch |= str_field(ui, "Root topic", &mut c.root);
            ch |= ui
                .checkbox(&mut c.encryption_enabled, "Send encrypted packets")
                .changed();
            ch |= ui.checkbox(&mut c.tls_enabled, "Use TLS").changed();
            ch |= ui
                .checkbox(
                    &mut c.proxy_to_client_enabled,
                    "Proxy MQTT through client (no direct internet on radio)",
                )
                .changed();
            ch |= ui
                .checkbox(
                    &mut c.map_reporting_enabled,
                    "Periodically report this node to the public mesh map",
                )
                .changed();
            if c.map_reporting_enabled {
                // map_report_settings is Option<MapReportSettings>; clone a
                // default if the firmware never reported one so the UI can
                // edit it without crashing.
                let settings = c
                    .map_report_settings
                    .get_or_insert_with(module_config::MapReportSettings::default);
                ch |= int_field(
                    ui,
                    "Map publish interval (s)",
                    &mut settings.publish_interval_secs,
                );
                ch |= int_field(
                    ui,
                    "Map position precision (bits)",
                    &mut settings.position_precision,
                );
                ch |= ui
                    .checkbox(
                        &mut settings.should_report_location,
                        "Opt-in: report location",
                    )
                    .changed();
            }
            ch
        },
        |svc, c| write_mqtt_fut(svc, c),
    );
}

fn write_mqtt_fut(svc: MeshtasticService, c: MqttConfig) -> ApplyFut {
    Box::pin(async move {
        svc.write_module_config(module_config::PayloadVariant::Mqtt(c))
            .await
            .map(|_| ())
    })
}

fn int_field(ui: &mut egui::Ui, label: &str, value: &mut u32) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        let resp = ui.add(egui::DragValue::new(value));
        if resp.changed() {
            changed = true;
        }
    });
    changed
}
