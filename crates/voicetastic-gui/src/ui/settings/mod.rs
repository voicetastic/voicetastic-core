//! Per-section device settings UI. Mirrors the layout and dirty-tracking
//! behaviour of the upstream Android `SettingsScreen`.
//!
//! Architecture: every editable section calls [`card`], which handles the
//! lock / load / dirty-mark / Apply-button boilerplate. A section body only
//! has to render fields against `&mut T` and provide a single-shot `write`
//! closure that ships `T` to the device.

mod enums;
mod widgets;

// Re-exported so the `impl_enum_strings!` macro can refer to the trait via a
// stable, crate-wide path regardless of where it's invoked.
pub(crate) use widgets::EnumStrings;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Runtime;

use voicetastic_core::Result as CoreResult;
use voicetastic_core::proto::{
    Position, channel,
    config::{
        self, bluetooth_config, device_config, display_config, lo_ra_config, network_config,
        position_config,
    },
};
use voicetastic_core::service::{ConnectionState, MeshService};

use crate::app::VoicetasticApp;
use crate::state::{FixedPosEdit, Section, SharedState};

use widgets::{
    bytes_to_hex, enum_combo, f32_field, f64_field, hex_to_bytes, i32_field, secret_field,
    str_field, u32_field,
};

// --------------------------------------------------------------------------
// Entry point + section context
// --------------------------------------------------------------------------

/// Bundles everything every section helper needs. Cheap to pass by reference.
struct Ctx<'a> {
    rt: &'a Arc<Runtime>,
    svc: &'a MeshService,
    shared: &'a Arc<Mutex<SharedState>>,
}

pub fn show(app: &mut VoicetasticApp, ui: &mut egui::Ui) {
    let ctx = Ctx {
        rt: &app.rt,
        svc: &app.service,
        shared: &app.shared,
    };

    egui::ScrollArea::vertical().show(ui, |ui| {
        connection_card(ui, &ctx);
        status_card(ui, &ctx);
        ui.add_space(4.0);

        owner_section(ui, &ctx);
        lora_section(ui, &ctx);
        device_section(ui, &ctx);
        position_section(ui, &ctx);
        power_section(ui, &ctx);
        network_section(ui, &ctx);
        display_section(ui, &ctx);
        bluetooth_section(ui, &ctx);
        channels_section(ui, &ctx);
        actions_section(ui, &ctx);
    });
}

// --------------------------------------------------------------------------
// Generic section card
// --------------------------------------------------------------------------

type ApplyFut = Pin<Box<dyn Future<Output = CoreResult<()>> + Send>>;

/// Static metadata describing a settings card.
struct CardMeta {
    title: &'static str,
    id: &'static str,
    section: Section,
    name: &'static str,
}

/// Render one collapsible config section.
///
/// Flow:
/// 1. Lock `SharedState`, call `get` to pull the latest snapshot. If `None`
///    (config not yet received), display a placeholder and return.
/// 2. Hand a `&mut T` to `render`. If anything was edited, mark `section`
///    dirty and store the new value back via `set`.
/// 3. On Apply, call `write(svc, value)`; on success clear the dirty flag
///    and post a status message.
fn card<T, R, W>(
    ui: &mut egui::Ui,
    ctx: &Ctx<'_>,
    meta: CardMeta,
    get: impl FnOnce(&SharedState) -> Option<T>,
    set: impl FnOnce(&mut SharedState, T),
    render: R,
    write: W,
) where
    T: Clone + Send + 'static,
    R: FnOnce(&mut egui::Ui, &mut T) -> bool,
    W: FnOnce(MeshService, T) -> ApplyFut + Send + 'static,
{
    egui::CollapsingHeader::new(meta.title)
        .id_salt(meta.id)
        .show(ui, |ui| {
            let cur = get(&ctx.shared.lock());
            let Some(mut value) = cur else {
                ui.label("(not received yet)");
                return;
            };
            if render(ui, &mut value) {
                let mut s = ctx.shared.lock();
                s.dirty.insert(meta.section);
                set(&mut s, value.clone());
            }
            ui.add_space(4.0);
            if ui.button(format!("Apply {}", meta.name)).clicked() {
                spawn_apply(ctx, meta.section, meta.name, value, write);
            }
        });
}

fn spawn_apply<T, W>(ctx: &Ctx<'_>, section: Section, name: &str, value: T, write: W)
where
    T: Send + 'static,
    W: FnOnce(MeshService, T) -> ApplyFut + Send + 'static,
{
    let svc = ctx.svc.clone();
    let shared = Arc::clone(ctx.shared);
    let name = name.to_string();
    ctx.rt.spawn(async move {
        let result = write(svc, value).await;
        let mut s = shared.lock();
        match result {
            Ok(()) => {
                s.dirty.remove(&section);
                s.config_status = Some(format!("{name} sent"));
            }
            Err(e) => s.config_status = Some(format!("{name} send failed: {e}")),
        }
    });
}

/// Tiny convenience for sections that write a `Config` payload variant.
fn write_config_fut(svc: MeshService, payload: config::PayloadVariant) -> ApplyFut {
    Box::pin(async move { svc.write_config(payload).await.map(|_| ()) })
}

// --------------------------------------------------------------------------
// Connection / status / actions
// --------------------------------------------------------------------------

fn connection_card(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    let (state, my_num, fw) = {
        let s = ctx.shared.lock();
        (
            s.conn_state,
            s.my_info.as_ref().map(|i| i.my_node_num),
            s.metadata.as_ref().map(|m| m.firmware_version.clone()),
        )
    };

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            let label = match state {
                ConnectionState::Disconnected => "⚪ Disconnected",
                ConnectionState::Connecting => "🟡 Connecting…",
                ConnectionState::Connected => "🟢 Connected",
                ConnectionState::Configuring => "🟡 Configuring…",
                ConnectionState::Ready => "🟢 Ready",
            };
            ui.heading(label);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⟳ Refresh config").clicked() {
                    let svc = ctx.svc.clone();
                    let shared = Arc::clone(ctx.shared);
                    ctx.rt.spawn(async move {
                        shared.lock().config_status = Some("Refresh requested…".into());
                        if let Err(e) = svc.refresh_config().await {
                            shared.lock().config_status = Some(format!("Refresh failed: {e}"));
                        }
                    });
                }
            });
        });
        if let Some(num) = my_num {
            ui.label(format!("Node: 0x{num:08x} ({num})"));
        }
        if let Some(v) = fw {
            ui.label(format!("Firmware: {v}"));
        }
    });
}

fn status_card(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    let status = ctx.shared.lock().config_status.clone();
    if let Some(msg) = status {
        ui.horizontal(|ui| {
            ui.label(msg);
            if ui.small_button("Dismiss").clicked() {
                ctx.shared.lock().config_status = None;
            }
        });
    }
}

fn actions_section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    egui::CollapsingHeader::new("⚠ Device actions")
        .id_salt("actions")
        .show(ui, |ui| {
            if ui.button("Reboot device (5s)").clicked() {
                run_status(ctx, "Reboot", |svc| {
                    Box::pin(async move { svc.reboot(5).await.map(|_| ()) })
                });
            }
            ui.add_space(4.0);
            let red = ui.style().visuals.error_fg_color;
            let btn = egui::Button::new(egui::RichText::new("Factory reset").color(red));
            if ui.add(btn).clicked() {
                run_status(ctx, "Factory reset", |svc| {
                    Box::pin(async move { svc.factory_reset().await.map(|_| ()) })
                });
            }
        });
}

/// Fire-and-forget admin call that just updates `config_status`.
fn run_status<F>(ctx: &Ctx<'_>, name: &str, fut_ctor: F)
where
    F: FnOnce(MeshService) -> ApplyFut + Send + 'static,
{
    let svc = ctx.svc.clone();
    let shared = Arc::clone(ctx.shared);
    let name = name.to_string();
    ctx.rt.spawn(async move {
        let result = fut_ctor(svc).await;
        shared.lock().config_status = Some(match result {
            Ok(()) => format!("{name} command sent"),
            Err(e) => format!("{name} failed: {e}"),
        });
    });
}

// --------------------------------------------------------------------------
// Sections
// --------------------------------------------------------------------------

fn owner_section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    card(
        ui,
        ctx,
        CardMeta {
            title: "👤 User / Owner",
            id: "owner",
            section: Section::Owner,
            name: "Owner",
        },
        |s| Some(s.owner.clone().unwrap_or_default()),
        |s, v| s.owner = Some(v),
        |ui, c| {
            let mut ch = false;
            ch |= str_field(ui, "Long name", &mut c.long_name);
            if str_field(ui, "Short name", &mut c.short_name) {
                c.short_name.truncate(4);
                ch = true;
            }
            ch |= ui.checkbox(&mut c.is_licensed, "Licensed (HAM)").changed();
            ch
        },
        |svc, c| Box::pin(async move { svc.write_owner(c).await.map(|_| ()) }),
    );
}

fn lora_section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
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

fn device_section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
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

/// Position section. Has two independent write paths:
///
/// 1. **PositionConfig** (`fixed_position`, broadcast intervals, GPS mode…)
///    written via `Config::Position`.
/// 2. **Fixed position coordinates** (lat/lon/alt) written via the
///    `SetFixedPosition` admin message — these live on the `Position`
///    message, not on `PositionConfig`. The Android upstream toggles the
///    boolean without ever sending the coordinates, which is the bug we're
///    fixing here. We seed the lat/lon/alt edit fields from our own
///    `NodeInfo.position` and surface them only when "Fixed position" is on.
fn position_section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    egui::CollapsingHeader::new("📍 Position")
        .id_salt("position")
        .show(ui, |ui| {
            // ---- PositionConfig editor --------------------------------
            let cur = ctx.shared.lock().position;
            let Some(mut c) = cur else {
                ui.label("(not received yet)");
                return;
            };
            let mut changed = false;
            changed |=
                enum_combo::<position_config::GpsMode>(ui, "GPS mode", &mut c.gps_mode, "pos_gps");
            changed |= ui
                .checkbox(&mut c.fixed_position, "Fixed position")
                .changed();
            changed |= ui
                .checkbox(&mut c.position_broadcast_smart_enabled, "Smart broadcast")
                .changed();
            changed |= u32_field(ui, "Broadcast interval (s)", &mut c.position_broadcast_secs);
            changed |= u32_field(ui, "GPS update interval (s)", &mut c.gps_update_interval);
            changed |= u32_field(
                ui,
                "Smart min distance (m)",
                &mut c.broadcast_smart_minimum_distance,
            );
            changed |= u32_field(
                ui,
                "Smart min interval (s)",
                &mut c.broadcast_smart_minimum_interval_secs,
            );
            if changed {
                let mut s = ctx.shared.lock();
                s.dirty.insert(Section::Position);
                s.position = Some(c);
            }
            ui.add_space(4.0);
            if ui.button("Apply Position Config").clicked() {
                spawn_apply(ctx, Section::Position, "Position Config", c, |svc, c| {
                    write_config_fut(svc, config::PayloadVariant::Position(c))
                });
            }

            // ---- Fixed coordinates (gated on the checkbox) ------------
            if !c.fixed_position {
                return;
            }
            ui.add_space(8.0);
            ui.separator();
            ui.label(egui::RichText::new("Fixed coordinates").strong());

            // Seed the edit buffer once from our own NodeInfo.position.
            {
                let mut s = ctx.shared.lock();
                if s.fixed_pos_edit.is_none() {
                    let seed = s
                        .my_info
                        .as_ref()
                        .map(|i| i.my_node_num)
                        .and_then(|num| s.nodes.get(&num))
                        .and_then(|n| n.position.as_ref())
                        .map(|p| FixedPosEdit {
                            latitude_deg: p.latitude_i.unwrap_or(0) as f64 * 1e-7,
                            longitude_deg: p.longitude_i.unwrap_or(0) as f64 * 1e-7,
                            altitude_m: p.altitude.unwrap_or(0),
                        })
                        .unwrap_or_default();
                    s.fixed_pos_edit = Some(seed);
                }
            }

            let mut edit = ctx.shared.lock().fixed_pos_edit.clone().unwrap_or_default();
            let mut e_changed = false;
            e_changed |= f64_field(ui, "Latitude (°)", &mut edit.latitude_deg);
            e_changed |= f64_field(ui, "Longitude (°)", &mut edit.longitude_deg);
            e_changed |= i32_field(ui, "Altitude (m)", &mut edit.altitude_m);
            if e_changed {
                ctx.shared.lock().fixed_pos_edit = Some(edit.clone());
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Send Fixed Position").clicked() {
                    let pos = Position {
                        latitude_i: Some((edit.latitude_deg * 1e7).round() as i32),
                        longitude_i: Some((edit.longitude_deg * 1e7).round() as i32),
                        altitude: Some(edit.altitude_m),
                        ..Default::default()
                    };
                    run_status(ctx, "Fixed position", move |svc| {
                        Box::pin(async move { svc.set_fixed_position(pos).await.map(|_| ()) })
                    });
                }
                if ui.button("Reset to current").clicked() {
                    ctx.shared.lock().fixed_pos_edit = None;
                }
            });
        });
}

fn power_section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
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

fn network_section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
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

fn display_section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
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

fn bluetooth_section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
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

// --------------------------------------------------------------------------
// Channels — list of independently editable rows. Doesn't fit `card<T>` so
// we open it manually but reuse the same field widgets and `spawn_apply`.
// --------------------------------------------------------------------------

fn channels_section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    egui::CollapsingHeader::new("💬 Channels")
        .id_salt("channels")
        .show(ui, |ui| {
            let channels = ctx.shared.lock().channels.clone();
            if channels.is_empty() {
                ui.label("(no channels received yet)");
                return;
            }
            for ch in channels {
                render_channel_row(ui, ctx, ch);
            }
        });
}

fn render_channel_row(ui: &mut egui::Ui, ctx: &Ctx<'_>, ch: voicetastic_core::proto::Channel) {
    let idx = ch.index;
    ui.group(|ui| {
        let role_name = channel::Role::try_from(ch.role)
            .map(|r| r.as_str_name().to_string())
            .unwrap_or_else(|_| format!("#{}", ch.role));
        ui.label(format!("Channel {idx} ({role_name})"));

        let mut next = ch.clone();
        let mut settings = next.settings.unwrap_or_default();
        let mut psk_hex = bytes_to_hex(&settings.psk);
        let mut changed = false;
        changed |= str_field(ui, "Name", &mut settings.name);
        if secret_field(ui, "PSK (hex)", &mut psk_hex, &format!("ch_psk_{idx}")) {
            if let Some(b) = hex_to_bytes(&psk_hex) {
                settings.psk = b;
            }
            changed = true;
        }
        changed |= ui
            .checkbox(&mut settings.uplink_enabled, "Uplink enabled")
            .changed();
        changed |= ui
            .checkbox(&mut settings.downlink_enabled, "Downlink enabled")
            .changed();
        next.settings = Some(settings);

        let section = Section::Channel(idx as u32);
        if changed {
            let mut s = ctx.shared.lock();
            s.dirty.insert(section);
            if let Some(slot) = s.channels.iter_mut().find(|c| c.index == idx) {
                *slot = next.clone();
            }
        }
        if ui.button(format!("Apply Channel {idx}")).clicked() {
            spawn_apply(ctx, section, &format!("Channel {idx}"), next, |svc, c| {
                Box::pin(async move { svc.write_channel(c).await.map(|_| ()) })
            });
        }
    });
}
