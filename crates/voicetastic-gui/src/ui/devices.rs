use std::sync::Arc;

use eframe::egui;
use voicetastic_core::settings::AppSettings;

use crate::app::VoicetasticApp;

pub fn show(app: &mut VoicetasticApp, ui: &mut egui::Ui) {
    ui.heading("Device Connection");
    ui.separator();

    ui.horizontal(|ui| {
        ui.label("Address / port:");
        ui.text_edit_singleline(&mut app.device_addr);
        if ui.button("Connect").clicked() {
            let addr = app.device_addr.clone();
            // Persist the address best-effort. Failure to write the config
            // file must never block the connection attempt.
            let _ = AppSettings {
                last_device: Some(addr.clone()),
            }
            .save();
            let svc = app.service.clone();
            let shared = Arc::clone(&app.shared);
            app.rt.spawn(async move {
                shared.lock().status_msg = Some("Connecting…".into());
                let result = if voicetastic_core::serial::is_serial_path(&addr) {
                    svc.connect_by_serial(&addr).await
                } else {
                    svc.connect_by_address(&addr).await
                };
                match result {
                    Ok(()) => {
                        shared.lock().status_msg = Some("Connected!".into());
                    }
                    Err(e) => {
                        shared.lock().status_msg = Some(format!("Connection failed: {e}"));
                    }
                }
            });
        }
        if ui.button("Disconnect").clicked() {
            let svc = app.service.clone();
            let shared = Arc::clone(&app.shared);
            app.rt.spawn(async move {
                if let Err(e) = svc.disconnect().await {
                    shared.lock().status_msg = Some(format!("Disconnect failed: {e}"));
                }
            });
        }
    });

    // Serial ports
    let serial_ports = voicetastic_core::serial::available_ports();
    if !serial_ports.is_empty() {
        ui.separator();
        ui.label("Serial ports:");
        for p in &serial_ports {
            let label = p.to_string_lossy();
            if ui.button(label.as_ref()).clicked() {
                app.device_addr = p.to_string_lossy().into_owned();
            }
        }
    }

    // BLE Scan
    ui.horizontal(|ui| {
        let scanning = app.shared.lock().scanning;
        if !scanning {
            if ui.button("Scan").clicked() {
                let svc = app.service.clone();
                let shared = Arc::clone(&app.shared);
                let ctx = ui.ctx().clone();
                app.rt.spawn(async move {
                    shared.lock().scanning = true;
                    shared.lock().scan_results.clear();
                    match svc.scan().await {
                        Ok(mut rx) => {
                            while let Some(dev) = rx.recv().await {
                                shared.lock().scan_results.push(dev);
                                ctx.request_repaint();
                            }
                        }
                        Err(e) => {
                            shared.lock().status_msg = Some(format!("Scan failed: {e}"));
                        }
                    }
                    shared.lock().scanning = false;
                    ctx.request_repaint();
                });
            }
        } else if ui.button("Stop Scan").clicked() {
            let svc = app.service.clone();
            app.rt.spawn(async move {
                let _ = svc.stop_scan().await;
            });
        }
    });

    if let Some(msg) = app.shared.lock().status_msg.clone() {
        ui.horizontal(|ui| {
            ui.label(&msg);
            if ui.small_button("Dismiss").clicked() {
                app.shared.lock().status_msg = None;
            }
        });
    }

    ui.separator();
    ui.heading("Discovered Devices");
    let results = app.shared.lock().scan_results.clone();
    for dev in &results {
        let label = dev.name.as_deref().unwrap_or(&dev.address);
        if ui.button(label).clicked() {
            app.device_addr = dev.address.clone();
        }
    }

    ui.separator();
    ui.heading("Known Nodes");
    let nodes = app.shared.lock().nodes.clone();
    egui::ScrollArea::vertical().show(ui, |ui| {
        for (num, node) in &nodes {
            let user_name = node
                .user
                .as_ref()
                .map(|u| u.long_name.as_str())
                .unwrap_or("?");
            ui.label(format!("  {user_name}  (0x{num:08x})"));
        }
    });
}
