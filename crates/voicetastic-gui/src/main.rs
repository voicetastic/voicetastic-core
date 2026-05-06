use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Runtime;
use tracing_subscriber::EnvFilter;

use voicetastic_core::ble::DiscoveredDevice;
use voicetastic_core::proto::{MyNodeInfo, NodeInfo};
use voicetastic_core::service::{ConnectionState, MeshService};

// ---------------------------------------------------------------------------
// Shared state (written by background tasks, read by UI)
// ---------------------------------------------------------------------------

struct SharedState {
    conn_state: ConnectionState,
    my_info: Option<MyNodeInfo>,
    nodes: HashMap<u32, NodeInfo>,
    chat_log: Vec<ChatEntry>,
    scan_results: Vec<DiscoveredDevice>,
    scanning: bool,
    status_msg: Option<String>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            conn_state: ConnectionState::Disconnected,
            my_info: None,
            nodes: HashMap::new(),
            chat_log: Vec::new(),
            scan_results: Vec::new(),
            scanning: false,
            status_msg: None,
        }
    }
}

#[derive(Clone)]
struct ChatEntry {
    from_id: String,
    text: String,
    #[allow(dead_code)]
    rx_time: u32,
    outgoing: bool,
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct VoicetasticApp {
    rt: Arc<Runtime>,
    service: MeshService,
    shared: Arc<Mutex<SharedState>>,
    tab: Tab,
    // Devices tab
    device_addr: String,
    // Chat tab
    chat_input: String,
    chat_channel: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Devices,
    Chat,
    Settings,
}

impl VoicetasticApp {
    fn new(cc: &eframe::CreationContext<'_>, rt: Arc<Runtime>, service: MeshService) -> Self {
        let shared = Arc::new(Mutex::new(SharedState::default()));

        // Spawn watchers that funnel service state → SharedState
        spawn_watchers(&rt, &service, Arc::clone(&shared), cc.egui_ctx.clone());

        Self {
            rt,
            service,
            shared,
            tab: Tab::Devices,
            device_addr: String::new(),
            chat_input: String::new(),
            chat_channel: 0,
        }
    }
}

fn spawn_watchers(
    rt: &Runtime,
    svc: &MeshService,
    shared: Arc<Mutex<SharedState>>,
    ctx: egui::Context,
) {
    // Connection state
    {
        let mut rx = svc.watch_state();
        let s = Arc::clone(&shared);
        let c = ctx.clone();
        rt.spawn(async move {
            while rx.changed().await.is_ok() {
                s.lock().conn_state = *rx.borrow_and_update();
                c.request_repaint();
            }
        });
    }
    // My info
    {
        let mut rx = svc.watch_my_info();
        let s = Arc::clone(&shared);
        let c = ctx.clone();
        rt.spawn(async move {
            while rx.changed().await.is_ok() {
                s.lock().my_info = rx.borrow_and_update().clone();
                c.request_repaint();
            }
        });
    }
    // Nodes
    {
        let mut rx = svc.watch_nodes();
        let s = Arc::clone(&shared);
        let c = ctx.clone();
        rt.spawn(async move {
            while rx.changed().await.is_ok() {
                s.lock().nodes = rx.borrow_and_update().clone();
                c.request_repaint();
            }
        });
    }
    // Incoming text
    {
        let mut rx = svc.subscribe_text();
        let s = Arc::clone(&shared);
        let c = ctx.clone();
        rt.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        s.lock().chat_log.push(ChatEntry {
                            from_id: msg.from_id.clone(),
                            text: msg.text.clone(),
                            rx_time: msg.rx_time,
                            outgoing: false,
                        });
                        c.request_repaint();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(_) => {} // lagged
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

impl eframe::App for VoicetasticApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Devices, "Devices");
                ui.selectable_value(&mut self.tab, Tab::Chat, "Chat");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let state = self.shared.lock().conn_state;
                    let label = match state {
                        ConnectionState::Disconnected => "⚪ Disconnected",
                        ConnectionState::Connecting => "🟡 Connecting…",
                        ConnectionState::Connected => "🟢 Connected",
                        ConnectionState::Configuring => "🟡 Configuring…",
                        ConnectionState::Ready => "🟢 Ready",
                    };
                    ui.label(label);
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Devices => self.ui_devices(ui),
            Tab::Chat => self.ui_chat(ui),
            Tab::Settings => self.ui_settings(ui),
        });
    }
}

impl VoicetasticApp {
    // ----- Devices tab -----
    fn ui_devices(&mut self, ui: &mut egui::Ui) {
        ui.heading("Device Connection");
        ui.separator();

        ui.horizontal(|ui| {
            ui.label("Address / port:");
            ui.text_edit_singleline(&mut self.device_addr);
            if ui.button("Connect").clicked() {
                let addr = self.device_addr.clone();
                let svc = self.service.clone();
                let shared = Arc::clone(&self.shared);
                self.rt.spawn(async move {
                    shared.lock().status_msg = Some("Connecting…".into());
                    let result = if addr.starts_with('/') || addr.starts_with("COM") {
                        svc.connect_by_serial(&addr).await
                    } else {
                        svc.connect_by_address(&addr).await
                    };
                    match result {
                        Ok(()) => {
                            shared.lock().status_msg = Some("Connected!".into());
                        }
                        Err(e) => {
                            shared.lock().status_msg =
                                Some(format!("Connection failed: {e}"));
                        }
                    }
                });
            }
            if ui.button("Disconnect").clicked() {
                let svc = self.service.clone();
                let shared = Arc::clone(&self.shared);
                self.rt.spawn(async move {
                    if let Err(e) = svc.disconnect().await {
                        shared.lock().status_msg =
                            Some(format!("Disconnect failed: {e}"));
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
                    self.device_addr = p.to_string_lossy().into_owned();
                }
            }
        }

        // BLE Scan
        ui.horizontal(|ui| {
            let scanning = self.shared.lock().scanning;
            if !scanning {
                if ui.button("Scan").clicked() {
                    let svc = self.service.clone();
                    let shared = Arc::clone(&self.shared);
                    let ctx = ui.ctx().clone();
                    self.rt.spawn(async move {
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
                                shared.lock().status_msg =
                                    Some(format!("Scan failed: {e}"));
                            }
                        }
                        shared.lock().scanning = false;
                        ctx.request_repaint();
                    });
                }
            } else if ui.button("Stop Scan").clicked() {
                let svc = self.service.clone();
                self.rt.spawn(async move {
                    let _ = svc.stop_scan().await;
                });
            }
        });

        if let Some(msg) = self.shared.lock().status_msg.clone() {
            ui.label(&msg);
        }

        ui.separator();
        ui.heading("Discovered Devices");
        let results = self.shared.lock().scan_results.clone();
        for dev in &results {
            let label = dev
                .name
                .as_deref()
                .unwrap_or(&dev.address);
            if ui.button(label).clicked() {
                self.device_addr = dev.address.clone();
            }
        }

        ui.separator();
        ui.heading("Known Nodes");
        let nodes = self.shared.lock().nodes.clone();
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

    // ----- Chat tab -----
    fn ui_chat(&mut self, ui: &mut egui::Ui) {
        ui.heading("Text Chat");
        ui.separator();

        // Messages scroll area
        let log = self.shared.lock().chat_log.clone();
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .max_height(ui.available_height() - 40.0)
            .show(ui, |ui| {
                for entry in &log {
                    let prefix = if entry.outgoing { "→ You" } else { &entry.from_id };
                    ui.label(format!("{prefix}: {}", entry.text));
                }
            });

        // Input
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Ch:");
            ui.add(egui::DragValue::new(&mut self.chat_channel).range(0..=7));
            let resp = ui.text_edit_singleline(&mut self.chat_input);
            if (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                || ui.button("Send").clicked()
            {
                let text = self.chat_input.clone();
                if !text.is_empty() {
                    self.chat_input.clear();
                    let svc = self.service.clone();
                    let ch = self.chat_channel;
                    let shared = Arc::clone(&self.shared);
                    self.rt.spawn(async move {
                        match svc.send_text(&text, ch, None).await {
                            Ok(_id) => {
                                shared.lock().chat_log.push(ChatEntry {
                                    from_id: String::new(),
                                    text,
                                    rx_time: 0,
                                    outgoing: true,
                                });
                            }
                            Err(e) => {
                                shared.lock().status_msg =
                                    Some(format!("Send failed: {e}"));
                            }
                        }
                    });
                }
            }
        });
    }

    // ----- Settings tab -----
    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.separator();

        let info = self.shared.lock().my_info.clone();
        if let Some(info) = info {
            ui.label(format!("My node num: {}", info.my_node_num));
        } else {
            ui.label("Not connected");
        }

        ui.separator();
        ui.label("Voice codec: AMR-NB (save/load .amr files only)");
        ui.label("No live microphone support in this version.");
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let rt = Arc::new(
        Runtime::new().expect("failed to create tokio runtime"),
    );

    let service = rt.block_on(async { MeshService::new().await.expect("MeshService::new") });

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 600.0])
            .with_min_inner_size([400.0, 300.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Voicetastic",
        native_options,
        Box::new(move |cc| Ok(Box::new(VoicetasticApp::new(cc, rt, service)))),
    )
}
