use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Runtime;
use tracing::error;

use voicetastic_core::MeshtasticService;
use voicetastic_core::meshtastic::service::ConnectionState;
use voicetastic_core::settings::{
    SettingKey, SettingsApi, SettingsListener, ThemeContrastKind, ThemeModeKind,
};
use voicetastic_core::voice::{AssemblerConfig, VoiceAssembler, VoiceSender};
use voicetastic_tokens::{ColorMode, Contrast};

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
    pub service: MeshtasticService,
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
    /// Centralised settings facade. Cloned `Arc` shared with watchers /
    /// listeners; UI widgets call its typed setters and never poke the
    /// underlying `AppSettings` directly.
    pub settings: Arc<SettingsApi>,
    /// Shared outbound voice pipeline. Owns the build → register →
    /// burst → NACK-driven retransmit loop; the chat composer hands
    /// it a `SendRequest` and subscribes to `SendStatus` events. One
    /// instance per service handle.
    pub voice_sender: Arc<VoiceSender>,
}

impl VoicetasticApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        rt: Arc<Runtime>,
        service: MeshtasticService,
    ) -> Self {
        // Install the shared design-token-driven theme. Static M3 tokens
        // (no motion / no dynamic color) parsed from
        // [`tokens/design.toml`](../../../tokens/design.toml) by the
        // `voicetastic-tokens` crate. `egui_style` returns a full
        // [`egui::Style`] (visuals + spacing + type scale) so colours,
        // padding rhythm, and the M3 type ramp all switch together.
        //
        // The mode (light/dark/system) and contrast tier (standard/high)
        // both come from `SettingsApi`; default is Dark + Standard which
        // matches the historical hard-coded startup pin. Changes via the
        // Appearance settings panel route back through `ThemeListener`
        // which re-runs `apply_theme` on the egui context.
        let settings = SettingsApi::open();
        apply_theme(&cc.egui_ctx, &settings);
        // Subscribe a listener so any later change (this UI, CLI, or
        // another front-end sharing the config file) re-applies the
        // egui styles + theme preference and requests a repaint.
        settings.subscribe(Arc::new(ThemeListener {
            ctx: cc.egui_ctx.clone(),
            settings: Arc::clone(&settings),
        }));

        let shared = Arc::new(Mutex::new(SharedState::default()));
        // NB: the `..AssemblerConfig::default()` spread is safe here because
        // this is the single writer of the config at construction time. Do
        // NOT copy this pattern into `apply_voice_settings` / `apply_lora_*`
        // — multiple call sites using the spread will silently clobber each
        // other's field contributions. Use `VoiceAssembler::update_config`
        // for in-place mutation after construction.
        let voice_assembler = Arc::new(VoiceAssembler::new({
            let mut cfg = AssemblerConfig {
                message_timeout: std::time::Duration::from_secs(
                    settings.reassembly_timeout_secs() as u64
                ),
                ..AssemblerConfig::default()
            };
            // Keep the consecutive-silence budget tied to
            // `message_timeout` so the user's reassembly slider
            // (10 s..=3600 s) is the real ceiling, not the static
            // `NACK_MAX_ROUNDS` constant.
            cfg.sync_nack_cap_to_timeout();
            cfg
        }));
        // `VoiceSender` background tasks run on the GUI's tokio
        // runtime; pass its handle explicitly so callers (UI thread,
        // settings listener) don't need an entered runtime context.
        let voice_sender = VoiceSender::new_on(service.clone(), rt.handle().clone());
        voice_sender.set_retain_ttl(std::time::Duration::from_secs(
            settings.reassembly_timeout_secs() as u64,
        ));
        // Auto-apply voice-runtime-affecting settings whenever they change
        // anywhere (UI, CLI sharing the same file, future Android host).
        settings.subscribe(Arc::new(VoiceRuntimeListener {
            assembler: Arc::clone(&voice_assembler),
            voice_sender: Arc::clone(&voice_sender),
            settings: Arc::clone(&settings),
            service: service.clone(),
        }));
        spawn_watchers(
            &rt,
            &service,
            Arc::clone(&shared),
            cc.egui_ctx.clone(),
            Arc::clone(&voice_assembler),
            Arc::clone(&settings),
        );

        let device_addr = settings.last_device().unwrap_or_default();

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
            settings,
            voice_sender,
        }
    }

    /// Resolve the outgoing-codec settings to a [`VoiceCodecParam`]
    /// suitable for [`Recorder::start`] and the voice protocol header.
    pub fn outgoing_voice_codec(&self) -> voicetastic_core::settings::VoiceCodecParam {
        self.settings.voice_codec_for_protocol()
    }
}

/// Install the current theme on the egui context: both [`egui::Style`]
/// slots (Light & Dark) get the token-driven style at the selected
/// contrast tier, and [`egui::ThemePreference`] is pinned according to
/// the user's mode (`system` defers to the host).
///
/// Cheap enough to call on every theme change (~one BTreeMap allocation
/// per slot — egui caches the style internally).
fn apply_theme(ctx: &egui::Context, settings: &SettingsApi) {
    let contrast = match settings.theme_contrast() {
        ThemeContrastKind::Standard => Contrast::Standard,
        ThemeContrastKind::High => Contrast::High,
    };
    // Populate both slots regardless of mode so an OS that flips
    // light/dark mid-session (System mode) is already styled correctly
    // on the next frame.
    ctx.set_style_of(
        egui::Theme::Light,
        voicetastic_tokens::egui_style_with_contrast(ColorMode::Light, contrast),
    );
    ctx.set_style_of(
        egui::Theme::Dark,
        voicetastic_tokens::egui_style_with_contrast(ColorMode::Dark, contrast),
    );
    let pref = match settings.theme_mode() {
        ThemeModeKind::System => egui::ThemePreference::System,
        ThemeModeKind::Light => egui::ThemePreference::Light,
        ThemeModeKind::Dark => egui::ThemePreference::Dark,
    };
    ctx.set_theme(pref);
}

/// Bridges theme-related [`SettingsApi`] events to the egui context.
/// Stays in `app.rs` rather than `watchers.rs` because it never touches
/// the voice runtime — only the UI presentation.
struct ThemeListener {
    ctx: egui::Context,
    settings: Arc<SettingsApi>,
}

impl SettingsListener for ThemeListener {
    fn on_change(&self, key: SettingKey) {
        // Filter early — listeners get every key, but only two of them
        // affect rendering. Re-applying styles is cheap, but issuing a
        // repaint for every voice-codec tweak would be wasteful.
        if !matches!(key, SettingKey::ThemeMode | SettingKey::ThemeContrast) {
            return;
        }
        apply_theme(&self.ctx, &self.settings);
        self.ctx.request_repaint();
    }
}

/// Bridges [`SettingsApi`] change notifications to the live voice
/// runtime so updating either field via *any* front-end (GUI widget,
/// CLI `settings set`, Android host) immediately reflects on the wire.
struct VoiceRuntimeListener {
    assembler: Arc<VoiceAssembler>,
    voice_sender: Arc<VoiceSender>,
    settings: Arc<SettingsApi>,
    service: MeshtasticService,
}

impl SettingsListener for VoiceRuntimeListener {
    fn on_change(&self, key: SettingKey) {
        match key {
            SettingKey::VoiceReassemblyTimeoutSecs => {
                let timeout =
                    std::time::Duration::from_secs(self.settings.reassembly_timeout_secs() as u64);
                // Mutate in place — do NOT use `set_config(... ..default())`
                // here, because [`watchers::apply_lora_to_assembler`] writes
                // the preset-derived `nack_window` and would clobber it (and
                // vice versa). See `VoiceAssembler::update_config` rustdoc.
                if let Err(e) = self.assembler.update_config(|cfg| {
                    cfg.message_timeout = timeout;
                    // Keep the consecutive-silence budget aligned with the
                    // new timeout so the round cap doesn't trip first.
                    cfg.sync_nack_cap_to_timeout();
                }) {
                    error!("Failed to update assembler config: {}", e);
                }
                // Keep the sender-side retransmit registry's retention
                // aligned with the receiver's reassembly window so a NACK
                // can never arrive for a frame we've already forgotten.
                self.voice_sender.set_retain_ttl(timeout);
            }
            SettingKey::VoiceNackMode => {
                // Re-resolve the NACK aggressiveness policy against the
                // current modem preset and push the new window /
                // backoff / round cap into the assembler.
                let preset = self
                    .service
                    .watch_lora_config()
                    .borrow()
                    .as_ref()
                    .and_then(|l| {
                        voicetastic_core::meshtastic::service::modem_preset_from_proto(
                            l.modem_preset,
                        )
                    });
                let params = self.settings.voice_nack_mode().resolve(preset);
                if let Err(e) = self.assembler.update_config(|cfg| {
                    cfg.nack_window = params.nack_window;
                    cfg.nack_backoff_base = params.backoff_base;
                    cfg.max_nack_rounds = params.max_nack_rounds;
                    if params.backoff_base != 0 {
                        cfg.sync_nack_cap_to_timeout();
                    }
                }) {
                    error!("Failed to update assembler nack params: {}", e);
                }
            }
            _ => {}
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

        #[cfg(target_os = "linux")]
        self.show_pairing_modal(ui.ctx());
    }
}

#[cfg(target_os = "linux")]
impl VoicetasticApp {
    /// Render the BlueZ passkey-entry modal when a pairing prompt is
    /// pending. The radio displays the 6-digit passkey on its OLED; the
    /// user types it here and we reply over the `org.bluez.Agent1`
    /// channel that's parked on a `oneshot`.
    fn show_pairing_modal(&self, ctx: &egui::Context) {
        use voicetastic_core::pairing::{PairingPromptKind, PairingResponse};

        // Snapshot what we need to render without holding the lock
        // across the egui call (the modal's input field writes back
        // under a fresh short-lived lock).
        let snapshot = {
            let st = self.shared.lock();
            st.pending_pairing
                .as_ref()
                .map(|p| (p.address.clone(), p.kind.clone(), p.input.clone()))
        };
        let Some((address, kind, mut input)) = snapshot else {
            return;
        };

        let mut submit: Option<PairingResponse> = None;
        let mut cancel = false;

        egui::Window::new("Pair Meshtastic radio")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("Device: {address}"));
                ui.add_space(8.0);
                match &kind {
                    PairingPromptKind::Passkey | PairingPromptKind::PinCode => {
                        let label = if matches!(kind, PairingPromptKind::Passkey) {
                            "Enter the 6-digit passkey shown on the radio's display:"
                        } else {
                            "Enter the PIN shown on the radio's display:"
                        };
                        ui.label(label);
                        ui.add(
                            egui::TextEdit::singleline(&mut input)
                                .desired_width(120.0)
                                .hint_text("123456"),
                        );
                        ui.horizontal(|ui| {
                            let submit_clicked = ui.button("Pair").clicked();
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                            if submit_clicked {
                                submit = match kind {
                                    PairingPromptKind::Passkey => input
                                        .trim()
                                        .parse::<u32>()
                                        .ok()
                                        .map(PairingResponse::Passkey),
                                    PairingPromptKind::PinCode => {
                                        Some(PairingResponse::Pin(input.trim().to_string()))
                                    }
                                    _ => None,
                                };
                            }
                        });
                    }
                    PairingPromptKind::Confirmation(passkey) => {
                        ui.label(format!(
                            "Does the radio display this passkey?\n\n    {passkey:06}"
                        ));
                        ui.horizontal(|ui| {
                            if ui.button("Yes, pair").clicked() {
                                submit = Some(PairingResponse::Confirm(true));
                            }
                            if ui.button("No").clicked() {
                                submit = Some(PairingResponse::Confirm(false));
                            }
                        });
                    }
                    PairingPromptKind::Authorization { uuid } => {
                        ui.label(format!("Authorise service {uuid} for this device?"));
                        ui.horizontal(|ui| {
                            if ui.button("Allow").clicked() {
                                submit = Some(PairingResponse::Confirm(true));
                            }
                            if ui.button("Deny").clicked() {
                                submit = Some(PairingResponse::Confirm(false));
                            }
                        });
                    }
                }
            });

        // Persist the in-progress input back into shared state.
        {
            let mut st = self.shared.lock();
            if let Some(p) = st.pending_pairing.as_mut() {
                p.input = input;
            }
        }

        if cancel {
            submit = Some(PairingResponse::Cancel);
        }
        if let Some(resp) = submit {
            let mut st = self.shared.lock();
            if let Some(mut p) = st.pending_pairing.take()
                && let Some(reply) = p.reply.take()
            {
                let _ = reply.send(resp);
            }
        }
    }
}
