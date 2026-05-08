//! `voicetastic scan` — list serial ports and discoverable BLE devices.

use std::time::Duration;

use anyhow::Result;
use tracing::info;

use voicetastic_core::ble::DiscoveredDevice;
use voicetastic_core::service::MeshService;

pub async fn run(seconds: u64) -> Result<()> {
    // Show available serial ports first
    let serial_ports = voicetastic_core::serial::available_ports();
    if !serial_ports.is_empty() {
        println!("Serial ports:");
        for p in &serial_ports {
            println!("  {}", p.display());
        }
        println!();
    }

    let svc = MeshService::new().await?;
    let mut rx = svc.scan().await?;
    info!(seconds, "scanning for BLE Meshtastic devices");
    println!("BLE devices:");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut seen = std::collections::HashSet::new();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            maybe = rx.recv() => {
                let Some(DiscoveredDevice { id: _, name, address }) = maybe else { break };
                if seen.insert(address.clone()) {
                    println!("{}\t{}", address, name.as_deref().unwrap_or("(unnamed)"));
                }
            }
        }
    }
    let _ = svc.stop_scan().await;
    Ok(())
}
