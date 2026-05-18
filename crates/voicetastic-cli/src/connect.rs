//! Shared connection logic: discover (BLE) or open (serial) a device and
//! wait for `config_complete`.

use std::time::Duration;

use anyhow::{Result, bail};
use tracing::info;

use voicetastic_core::MeshtasticService;
use voicetastic_core::meshtastic::service::ConnectionState;

use crate::util::is_serial;

pub async fn connect(svc: &MeshtasticService, device: &str) -> Result<()> {
    if is_serial(device) {
        info!(port = device, "connecting via serial");
        svc.connect_by_serial(device).await?;
    } else {
        // Drain BlueZ pairing prompts from a background task so a
        // PIN-required radio can be brought online from the CLI. The
        // task reads the passkey from stdin and replies to BlueZ via
        // the agent's oneshot channel. Best-effort: on platforms /
        // builds where the agent isn't registered, this simply does
        // nothing.
        #[cfg(target_os = "linux")]
        spawn_pairing_prompt_handler(svc.clone());

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

/// Background task that drains `MeshtasticService::pairing_prompts()` and
/// answers each prompt by reading from the controlling terminal.
///
/// Spawned by [`connect`] before the BLE link is brought up so the
/// Meshtastic radio's first `RequestPasskey` over DBus has a consumer
/// waiting. The task exits once the receiver is closed (i.e. when the
/// `MeshService` drops, normally at process exit).
///
/// Best-effort: if the agent failed to register at `MeshtasticService::new`
/// (e.g. headless system with no DBus session, or another process
/// holds the default agent), `pairing_prompts()` returns `None` and
/// we silently no-op. The user will then see the usual
/// "BlueZ pairing agent not registered" error from `prepare_link`.
#[cfg(target_os = "linux")]
fn spawn_pairing_prompt_handler(svc: MeshtasticService) {
    use std::io::{Write, stderr, stdin};
    use voicetastic_core::pairing::{PairingPromptKind, PairingResponse};

    tokio::spawn(async move {
        let Some(mut rx) = svc.pairing_prompts().await else {
            return;
        };
        while let Some(prompt) = rx.recv().await {
            // Read from stdin on a blocking thread so we don't stall
            // the tokio runtime, and so the user can take their time
            // reading the OLED.
            let kind = prompt.kind.clone();
            let address = prompt.address.clone();
            let response = tokio::task::spawn_blocking(move || {
                let mut err = stderr().lock();
                match &kind {
                    PairingPromptKind::Passkey => {
                        let _ = writeln!(
                            err,
                            "\n>>> Meshtastic radio {address} is requesting a BLE passkey."
                        );
                        let _ = write!(
                            err,
                            ">>> Enter the 6-digit passkey shown on the radio's OLED (or blank to cancel): "
                        );
                        let _ = err.flush();
                        let mut buf = String::new();
                        if stdin().read_line(&mut buf).is_err() {
                            return PairingResponse::Cancel;
                        }
                        match buf.trim().parse::<u32>() {
                            Ok(p) => PairingResponse::Passkey(p),
                            Err(_) => PairingResponse::Cancel,
                        }
                    }
                    PairingPromptKind::PinCode => {
                        let _ = writeln!(
                            err,
                            "\n>>> Meshtastic radio {address} is requesting a BLE PIN."
                        );
                        let _ = write!(err, ">>> Enter the PIN (or blank to cancel): ");
                        let _ = err.flush();
                        let mut buf = String::new();
                        if stdin().read_line(&mut buf).is_err() {
                            return PairingResponse::Cancel;
                        }
                        let pin = buf.trim().to_string();
                        if pin.is_empty() {
                            PairingResponse::Cancel
                        } else {
                            PairingResponse::Pin(pin)
                        }
                    }
                    PairingPromptKind::Confirmation(passkey) => {
                        let _ = writeln!(
                            err,
                            "\n>>> Radio {address} displays passkey {passkey:06}."
                        );
                        let _ = write!(err, ">>> Does it match? [y/N]: ");
                        let _ = err.flush();
                        let mut buf = String::new();
                        if stdin().read_line(&mut buf).is_err() {
                            return PairingResponse::Confirm(false);
                        }
                        PairingResponse::Confirm(matches!(
                            buf.trim().to_ascii_lowercase().as_str(),
                            "y" | "yes"
                        ))
                    }
                    PairingPromptKind::Authorization { uuid } => {
                        let _ = writeln!(
                            err,
                            "\n>>> Radio {address} requests authorisation for service {uuid}."
                        );
                        let _ = write!(err, ">>> Allow? [y/N]: ");
                        let _ = err.flush();
                        let mut buf = String::new();
                        if stdin().read_line(&mut buf).is_err() {
                            return PairingResponse::Confirm(false);
                        }
                        PairingResponse::Confirm(matches!(
                            buf.trim().to_ascii_lowercase().as_str(),
                            "y" | "yes"
                        ))
                    }
                }
            })
            .await
            .unwrap_or(PairingResponse::Cancel);
            let _ = prompt.reply.send(response);
        }
    });
}
