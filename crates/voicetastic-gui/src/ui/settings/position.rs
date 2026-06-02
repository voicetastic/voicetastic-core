//! Position section. Has two independent write paths:
//!
//! 1. **PositionConfig** (`fixed_position`, broadcast intervals, GPS mode…)
//!    written via `Config::Position`.
//! 2. **Fixed position coordinates** (lat/lon/alt) written via the
//!    `SetFixedPosition` admin message — these live on the `Position`
//!    message, not on `PositionConfig`. The Android upstream toggles the
//!    boolean without ever sending the coordinates, which is the bug we're
//!    fixing here. We seed the lat/lon/alt edit fields from our own
//!    `NodeInfo.position` and surface them only when "Fixed position" is on.

use eframe::egui;

use voicetastic_core::proto::{
    Position,
    config::{self, position_config},
};

use super::widgets::{enum_combo, f64_field, i32_field, u32_field};
use super::{Ctx, run_status, spawn_apply, write_config_fut};
use crate::state::{FixedPosEdit, Section};

pub(super) fn section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
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

            // Seed the edit buffer (once) from our own NodeInfo.position, then
            // hand back a working copy. A single lock acquisition keeps the
            // init + read atomic so the UI never observes a half-initialised
            // state when watchers concurrently update `nodes`.
            let mut edit = {
                let mut s = ctx.shared.lock();
                if s.fixed_pos_edit.is_none() {
                    let seed = s
                        .my_info
                        .as_ref()
                        .map(|i| i.my_node_num)
                        .and_then(|num| s.nodes.get(&num))
                        .and_then(|n| n.position.as_ref())
                        .map(|p| FixedPosEdit {
                            // `unwrap_or(0)` here is intentional: the field is
                            // `Option<i32>` in the proto but the edit buffer
                            // needs a concrete starting value. Users can
                            // immediately overwrite via the input fields.
                            latitude_deg: p.latitude_i.unwrap_or(0) as f64 * 1e-7,
                            longitude_deg: p.longitude_i.unwrap_or(0) as f64 * 1e-7,
                            altitude_m: p.altitude.unwrap_or(0),
                        })
                        .unwrap_or_default();
                    s.fixed_pos_edit = Some(seed);
                }
                // Safe: we just ensured it is `Some`.
                s.fixed_pos_edit.clone().unwrap_or_default()
            };

            let mut e_changed = false;
            e_changed |= f64_field(ui, "Latitude (°)", &mut edit.latitude_deg);
            e_changed |= f64_field(ui, "Longitude (°)", &mut edit.longitude_deg);
            e_changed |= i32_field(ui, "Altitude (m)", &mut edit.altitude_m);
            if e_changed {
                // Clamp immediately so the input field re-seeds to a valid
                // value next frame, instead of silently fixing it on Send.
                edit.latitude_deg = edit.latitude_deg.clamp(-90.0, 90.0);
                edit.longitude_deg = edit.longitude_deg.clamp(-180.0, 180.0);
                ctx.shared.lock().fixed_pos_edit = Some(edit.clone());
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Send Fixed Position").clicked() {
                    // Defensive: re-clamp at send time too, in case anything
                    // else mutated the edit buffer between input and click.
                    let lat_deg = edit.latitude_deg.clamp(-90.0, 90.0);
                    let lon_deg = edit.longitude_deg.clamp(-180.0, 180.0);
                    let pos = Position {
                        latitude_i: Some((lat_deg * 1e7).round() as i32),
                        longitude_i: Some((lon_deg * 1e7).round() as i32),
                        altitude: Some(edit.altitude_m),
                        ..Default::default()
                    };
                    run_status(ctx, "Fixed position", move |svc| {
                        Box::pin(async move { svc.set_fixed_position(pos).await.map(|_| ()) })
                    });
                }
                if ui.button("Broadcast position now").clicked() {
                    // Broadcasts the edited coordinates as a one-shot
                    // Position packet on POSITION_APP. Distinct from
                    // "Send Fixed Position" above, which writes a config
                    // admin message to the local radio (and does not
                    // emit a mesh packet).
                    let lat_deg = edit.latitude_deg.clamp(-90.0, 90.0);
                    let lon_deg = edit.longitude_deg.clamp(-180.0, 180.0);
                    let pos = Position {
                        latitude_i: Some((lat_deg * 1e7).round() as i32),
                        longitude_i: Some((lon_deg * 1e7).round() as i32),
                        altitude: Some(edit.altitude_m),
                        ..Default::default()
                    };
                    run_status(ctx, "Broadcast position", move |svc| {
                        Box::pin(async move {
                            svc.broadcast_position(pos, 0, None).await.map(|_| ())
                        })
                    });
                }
                if ui.button("Reset to current").clicked() {
                    ctx.shared.lock().fixed_pos_edit = None;
                }
            });
        });
}
