//! Per-section device settings UI. Mirrors the layout and dirty-tracking
//! behaviour of the upstream Android `SettingsScreen`.
//!
//! Architecture: every editable section calls [`card`], which handles the
//! lock / load / dirty-mark / Apply-button boilerplate. A section body only
//! has to render fields against `&mut T` and provide a single-shot `write`
//! closure that ships `T` to the device.
//!
//! Each editable settings section lives in its own submodule so this entry
//! point stays small and the sections can evolve independently.

mod appearance;
mod bluetooth;
mod channels;
mod connection;
mod device;
mod display;
mod enums;
mod lora;
mod mqtt;
mod network;
mod owner;
mod position;
mod power;
mod voice;
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

use voicetastic_core::MeshtasticService;
use voicetastic_core::Result as CoreResult;
use voicetastic_core::proto::config;

use crate::app::VoicetasticApp;
use crate::state::{Section, SharedState};

// --------------------------------------------------------------------------
// Entry point + section context
// --------------------------------------------------------------------------

/// Bundles everything every section helper needs. Cheap to pass by reference.
pub(super) struct Ctx<'a> {
    pub(super) rt: &'a Arc<Runtime>,
    pub(super) svc: &'a MeshtasticService,
    pub(super) shared: &'a Arc<Mutex<SharedState>>,
}

pub fn show(app: &mut VoicetasticApp, ui: &mut egui::Ui) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        // Client-side preferences first — these don't need a device
        // connection and have no dirty-tracking complications.
        appearance::section(ui, app);
        voice::section(ui, app);
        ui.add_space(4.0);

        let ctx = Ctx {
            rt: &app.rt,
            svc: &app.service,
            shared: &app.shared,
        };

        connection::connection_card(ui, &ctx);
        connection::status_card(ui, &ctx);
        ui.add_space(4.0);

        owner::section(ui, &ctx);
        lora::section(ui, &ctx);
        device::section(ui, &ctx);
        position::section(ui, &ctx);
        power::section(ui, &ctx);
        network::section(ui, &ctx);
        display::section(ui, &ctx);
        bluetooth::section(ui, &ctx);
        mqtt::section(ui, &ctx);
        channels::section(ui, &ctx);
        connection::actions_section(ui, &ctx);
    });
}

// --------------------------------------------------------------------------
// Generic section card
// --------------------------------------------------------------------------

pub(super) type ApplyFut = Pin<Box<dyn Future<Output = CoreResult<()>> + Send>>;

/// Static metadata describing a settings card.
pub(super) struct CardMeta {
    pub title: &'static str,
    pub id: &'static str,
    pub section: Section,
    pub name: &'static str,
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
pub(super) fn card<T, R, W>(
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
    W: FnOnce(MeshtasticService, T) -> ApplyFut + Send + 'static,
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

pub(super) fn spawn_apply<T, W>(ctx: &Ctx<'_>, section: Section, name: &str, value: T, write: W)
where
    T: Send + 'static,
    W: FnOnce(MeshtasticService, T) -> ApplyFut + Send + 'static,
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
                let msg = format!("apply {name}");
                crate::watchers::push_debug(&mut *s, crate::state::DebugLevel::Info, "settings", msg);
            }
            Err(e) => {
                s.config_status = Some(format!("{name} send failed: {e}"));
                let msg = format!("apply {name} failed: {e}");
                crate::watchers::push_debug(&mut *s, crate::state::DebugLevel::Error, "settings", msg);
            }
        }
    });
}

/// Tiny convenience for sections that write a `Config` payload variant.
pub(super) fn write_config_fut(
    svc: MeshtasticService,
    payload: config::PayloadVariant,
) -> ApplyFut {
    Box::pin(async move { svc.write_config(payload).await.map(|_| ()) })
}

/// Fire-and-forget admin call that just updates `config_status`.
pub(super) fn run_status<F>(ctx: &Ctx<'_>, name: &str, fut_ctor: F)
where
    F: FnOnce(MeshtasticService) -> ApplyFut + Send + 'static,
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
