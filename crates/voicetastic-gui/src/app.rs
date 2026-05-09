use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Runtime;

use voicetastic_core::service::{ConnectionState, MeshService};
use voicetastic_core::settings::AppSettings;

use crate::state::{SharedState, Tab};
use crate::ui;
use crate::watchers::spawn_watchers;

pub struct VoicetasticApp {
    pub rt: Arc<Runtime>,
    pub service: MeshService,
    pub shared: Arc<Mutex<SharedState>>,
    pub tab: Tab,
    // Devices tab
    pub device_addr: String,
    // Chat tab
    pub chat_input: String,
    pub chat_channel: u32,
}

impl VoicetasticApp {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: Arc<Runtime>, service: MeshService) -> Self {
        let shared = Arc::new(Mutex::new(SharedState::default()));
        spawn_watchers(&rt, &service, Arc::clone(&shared), cc.egui_ctx.clone());

        let prefs = AppSettings::load();

        Self {
            rt,
            service,
            shared,
            tab: Tab::Devices,
            device_addr: prefs.last_device.unwrap_or_default(),
            chat_input: String::new(),
            chat_channel: 0,
        }
    }
}

impl eframe::App for VoicetasticApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("tabs").show_inside(ui, |ui| {
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

        egui::CentralPanel::default().show_inside(ui, |ui| match self.tab {
            Tab::Devices => ui::devices::show(self, ui),
            Tab::Chat => ui::chat::show(self, ui),
            Tab::Settings => ui::settings::show(self, ui),
        });
    }
}
