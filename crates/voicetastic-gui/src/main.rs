mod app;
mod state;
mod ui;
mod watchers;

use std::sync::Arc;

use eframe::egui;
use tokio::runtime::Runtime;
use tracing_subscriber::EnvFilter;

use voicetastic_core::service::MeshService;

use crate::app::VoicetasticApp;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let rt: Arc<Runtime> = Arc::new(Runtime::new().expect("failed to create tokio runtime"));

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
