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
            .with_min_inner_size([400.0, 300.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Voicetastic",
        native_options,
        Box::new(move |cc| Ok(Box::new(VoicetasticApp::new(cc, rt, service)))),
    )?;
    Ok(())
}
