use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Runtime;

use voicetastic_core::service::{ConnectionState, MeshService};
use voicetastic_core::settings::AppSettings;
use voicetastic_core::voice::{AssemblerConfig, VoiceAssembler};

use crate::outgoing::OutgoingVoiceRegistry;

use crate::state::{SharedState, Tab};
use crate::ui;
use crate::ui::chat::VoiceCompose;
use crate::watchers::spawn_watchers;

/// Identifies which voice clip is currently playing back so the chat tab
/// can render the inline player next to the right row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PlaybackSource {
    /// Index into `SharedState::chat_log`.
    LogEntry(usize),
    /// The clip currently held in the voice composer's preview state.
    Preview,
}

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
    /// Destination node num for outgoing messages. `None` = broadcast.
    pub chat_dest: Option<u32>,
    /// In-progress voice message composition state.
    pub voice_compose: VoiceCompose,
    /// Currently-playing inbound or outbound voice clip (single-track —
    /// hitting Play on another entry interrupts the current playback).
    pub voice_playback: Option<crate::audio::PlaybackHandle>,
    /// Identifies which clip the active `voice_playback` was started from,
    /// so the inline player widget can render next to the right row.
    pub playback_source: Option<PlaybackSource>,
    /// Latest status / error message from the chat tab (e.g. playback or
    /// mic failures). Shown inline under the voice composer.
    pub chat_status: Option<String>,
    /// Persistent app settings (last device, voice duration cap, …).
    /// Mutated in place by the UI; `save_settings()` flushes to disk.
    pub app_settings: AppSettings,
    /// Shared voice reassembler. Held here so settings changes can hot-
    /// reload its config via [`Self::apply_voice_settings`].
    pub voice_assembler: Arc<VoiceAssembler>,
    /// Registry of outgoing voice messages retained for NACK-driven
    /// retransmit. Populated by the chat composer, consumed by the
    /// inbound-data watcher.
    pub outgoing_voice: Arc<OutgoingVoiceRegistry>,
}

impl VoicetasticApp {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: Arc<Runtime>, service: MeshService) -> Self {
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let prefs = AppSettings::load();
        let voice_assembler = Arc::new(VoiceAssembler::new(AssemblerConfig {
            message_timeout: std::time::Duration::from_secs(prefs.reassembly_timeout_secs() as u64),
            ..AssemblerConfig::default()
        }));
        let outgoing_voice = OutgoingVoiceRegistry::new();
        outgoing_voice.set_retain_ttl(std::time::Duration::from_secs(
            prefs.reassembly_timeout_secs() as u64,
        ));
        spawn_watchers(
            &rt,
            &service,
            Arc::clone(&shared),
            cc.egui_ctx.clone(),
            Arc::clone(&voice_assembler),
            Arc::clone(&outgoing_voice),
        );

        let device_addr = prefs.last_device.clone().unwrap_or_default();

        Self {
            rt,
            service,
            shared,
            tab: Tab::Devices,
            device_addr,
            chat_input: String::new(),
            chat_channel: 0,
            chat_dest: None,
            voice_compose: VoiceCompose::Idle,
            voice_playback: None,
            playback_source: None,
            chat_status: None,
            app_settings: prefs,
            voice_assembler,
            outgoing_voice,
        }
    }

    /// Push the current `app_settings` values that affect the voice
    /// assembler into the shared instance. Cheap; safe to call on every
    /// slider change.
    pub fn apply_voice_settings(&self) {
        // Mutate in place — do NOT use `set_config(... ..default())`
        // here, because [`watchers::apply_lora_to_assembler`] writes the
        // preset-derived `nack_window` and would clobber it (and vice
        // versa). See `VoiceAssembler::update_config` rustdoc.
        let timeout =
            std::time::Duration::from_secs(self.app_settings.reassembly_timeout_secs() as u64);
        self.voice_assembler.update_config(|cfg| {
            cfg.message_timeout = timeout;
        });
        // Keep the sender-side retransmit registry's retention aligned
        // with the receiver's reassembly window so a NACK can never
        // arrive for a frame we've already forgotten.
        self.outgoing_voice.set_retain_ttl(timeout);
    }

    /// Persist `app_settings` to disk best-effort. Failures are logged but
    /// never surfaced — the UI must keep working on read-only filesystems.
    pub fn save_settings(&self) {
        if let Err(e) = self.app_settings.save() {
            tracing::warn!(error = %e, "failed to save app settings");
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
