//! `voicetastic text {send,listen}` — text-message commands.

use std::time::Duration;

use anyhow::{Result, bail};
use tracing::info;

use voicetastic_core::MeshtasticService;
use voicetastic_core::meshtastic::ack::AckResult;

use crate::connect::connect;
use crate::util::disconnect_with_timeout;

/// How long we wait for a DM delivery ack before reporting `TimedOut`.
/// The firmware acks DMs within a few hundred ms over BLE for direct
/// neighbours; multi-hop flood routing on slow LoRa presets can stretch
/// to a handful of seconds. 30 s covers the worst preset (`LongSlow`)
/// with several hops, without making `voicetastic text send --to` feel
/// stuck on the happy path.
const ACK_DEADLINE: Duration = Duration::from_secs(30);

pub async fn send(device: &str, channel: u32, to: Option<u32>, body: &str) -> Result<()> {
    let svc = MeshtasticService::new().await?;
    connect(&svc, device).await?;
    let ack = if let Some(dest) = to {
        let (id, handle) = svc.send_text_tracked(body, channel, dest).await?;
        println!("sent text id={id} to={dest:#010x}; waiting for delivery ack…");
        Some(handle.wait(ACK_DEADLINE).await)
    } else {
        let id = svc.send_text(body, channel, to).await?;
        println!("sent text id={id} (broadcast — no ack)");
        None
    };
    disconnect_with_timeout(&svc).await;
    match ack {
        None | Some(AckResult::Delivered) => {
            if matches!(ack, Some(AckResult::Delivered)) {
                println!("delivered");
            }
            Ok(())
        }
        Some(AckResult::Failed(e)) => {
            bail!("delivery failed: {e:?}");
        }
        Some(AckResult::TimedOut) => {
            bail!(
                "no delivery ack within {} s; packet may still be in flight",
                ACK_DEADLINE.as_secs(),
            );
        }
        Some(AckResult::Cancelled) => {
            bail!("service disconnected before delivery ack arrived");
        }
    }
}

pub async fn listen(device: &str) -> Result<()> {
    let svc = MeshtasticService::new().await?;
    connect(&svc, device).await?;
    let mut rx = svc.subscribe_text();
    info!("listening for text messages, ctrl-c to stop");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            msg = rx.recv() => match msg {
                Ok(t) => println!("[ch{} {} -> {}] {}", t.channel, t.from_id, t.to, t.text),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "text listener lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    disconnect_with_timeout(&svc).await;
    Ok(())
}
