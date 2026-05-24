mod app;
mod audio;
mod state;
mod ui;
mod watchers;

use std::error::Error;
use std::sync::Arc;

use eframe::egui;
use tokio::runtime::Runtime;
use tracing::error;
use tracing_subscriber::EnvFilter;

use voicetastic_core::MeshtasticService;

use crate::app::VoicetasticApp;

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let rt: Arc<Runtime> = Arc::new(Runtime::new().map_err(|e| {
        error!(error = %e, "failed to create tokio runtime");
        e
    })?);

    let service = rt
        .block_on(async { MeshtasticService::new().await })
        .map_err(|e| {
            error!(error = %e, "failed to initialise MeshService");
            e
        })?;

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 600.0])
            .with_min_inner_size([400.0, 300.0])
            .with_icon(load_window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "Voicetastic",
        native_options,
        Box::new(move |cc| Ok(Box::new(VoicetasticApp::new(cc, rt, service)))),
    )?;
    Ok(())
}

/// Decode the bundled launcher icon (PNG embedded at compile time) into
/// the RGBA buffer eframe expects. The asset lives in
/// [`crates/voicetastic-tokens/assets/`] alongside the SVG source so
/// every platform (egui, Android, web) consumes the same mark.
///
/// On decode failure we log + fall back to a transparent 1×1 icon
/// rather than refusing to start the app; the asset is bundled in the
/// binary so this is effectively unreachable in practice.
fn load_window_icon() -> egui::IconData {
    match image::load_from_memory_with_format(
        voicetastic_tokens::ICON_PNG_256,
        image::ImageFormat::Png,
    ) {
        Ok(img) => {
            let rgba = img.into_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            egui::IconData {
                rgba: rgba.into_raw(),
                width: w,
                height: h,
            }
        }
        Err(e) => {
            error!(error = %e, "failed to decode bundled window icon; window will start un-iconned");
            egui::IconData {
                rgba: vec![0, 0, 0, 0],
                width: 1,
                height: 1,
            }
        }
    }
}
