//! Shared connection logic: discover (BLE) or open (serial) a device and
//! wait for `config_complete`.

use std::time::Duration;

use anyhow::{Result, bail};
use tracing::info;

use voicetastic_core::service::{ConnectionState, MeshService};

use crate::util::is_serial;

pub async fn connect(svc: &MeshService, device: &str) -> Result<()> {
    if is_serial(device) {
        info!(port = device, "connecting via serial");
        svc.connect_by_serial(device).await?;
    } else {
        // BLE: briefly scan first so the adapter has a peripheral to look up.
        let mut rx = svc.scan().await?;
        let target = device.to_ascii_lowercase();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                maybe = rx.recv() => {
                    let Some(d) = maybe else { break };
                    if d.address.to_ascii_lowercase() == target { break; }
                }
            }
        }
        let _ = svc.stop_scan().await;
        svc.connect_by_address(device).await?;
    }

    let mut state = svc.watch_state();
    let ready = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if *state.borrow() == ConnectionState::Ready {
                return Ok::<_, anyhow::Error>(());
            }
            state.changed().await?;
        }
    })
    .await;
    match ready {
        Ok(Ok(())) => {
            info!("connected and configured");
            Ok(())
        }
        Ok(Err(e)) => Err(e),
        Err(_) => bail!("timed out waiting for config_complete"),
    }
}
