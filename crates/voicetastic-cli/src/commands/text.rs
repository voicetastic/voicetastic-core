//! `voicetastic text {send,listen}` — text-message commands.

use anyhow::Result;
use tracing::info;

use voicetastic_core::MeshtasticService;

use crate::connect::connect;

pub async fn send(device: &str, channel: u32, to: Option<u32>, body: &str) -> Result<()> {
    let svc = MeshtasticService::new().await?;
    connect(&svc, device).await?;
    let id = svc.send_text(body, channel, to).await?;
    println!("sent text id={id}");
    let _ = svc.disconnect().await;
    Ok(())
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
    let _ = svc.disconnect().await;
    Ok(())
}
