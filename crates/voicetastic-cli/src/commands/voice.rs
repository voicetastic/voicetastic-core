//! `voicetastic voice {send,listen}` — voice-message commands (raw AMR I/O).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{info, warn};

use voicetastic_core::ports::PRIVATE_APP;
use voicetastic_core::service::MeshService;
use voicetastic_core::voice::{
    AmrNbBitrate, AssemblyEvent, VoiceAssembler, VoiceChunk, VoiceChunker, VoiceConfig,
    VoiceMessage,
};

use crate::connect::connect;

pub async fn send(
    device: &str,
    channel: u32,
    to: Option<u32>,
    file: &Path,
    bitrate: AmrNbBitrate,
) -> Result<()> {
    let bytes = tokio::fs::read(file)
        .await
        .with_context(|| format!("reading {}", file.display()))?;
    let svc = MeshService::new().await?;
    connect(&svc, device).await?;
    let mut id_buf = [0u8; 2];
    getrandom::fill(&mut id_buf).map_err(|e| anyhow::anyhow!("rng: {e}"))?;
    let message_id: u16 = u16::from_ne_bytes(id_buf).max(1);
    let chunks = VoiceChunker::chunk(&bytes, message_id, bitrate)?;
    info!(chunks = chunks.len(), "sending voice");
    let ids = svc.send_voice_chunks(chunks, channel, to).await?;
    println!("sent voice message_id={message_id}, packet_ids={:?}", ids);
    let _ = svc.disconnect().await;
    Ok(())
}

pub async fn listen(device: &str, out_dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(out_dir).await.ok();
    let svc = MeshService::new().await?;
    connect(&svc, device).await?;
    let assembler = VoiceAssembler::new(&VoiceConfig::default());
    let mut rx = svc.subscribe_data();
    info!("listening for voice messages, ctrl-c to stop");
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tick.tick() => {
                for completed in assembler.tick() {
                    save_amr(out_dir, &completed).await?;
                }
            }
            data = rx.recv() => match data {
                Ok(d) => {
                    if d.portnum != PRIVATE_APP as i32 { continue; }
                    let from_id = voicetastic_core::ids::node_num_to_id(d.from);
                    let to_id = voicetastic_core::ids::node_num_to_id(d.to);
                    let chunk = match VoiceChunk::parse(&d.payload) {
                        Ok(c) => c,
                        Err(e) => { warn!(?e, "bad voice chunk"); continue; }
                    };
                    match assembler.accept(&from_id, &to_id, d.channel, chunk) {
                        AssemblyEvent::Complete(msg) => save_amr(out_dir, &msg).await?,
                        AssemblyEvent::Pending => {}
                        AssemblyEvent::Duplicate => {}
                        AssemblyEvent::Rejected => warn!("rejected voice chunk"),
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "voice listener lagged, dropped chunks");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    let _ = svc.disconnect().await;
    Ok(())
}

async fn save_amr(out_dir: &Path, msg: &VoiceMessage) -> Result<()> {
    let path = out_dir.join(format!(
        "{}_{}.amr",
        msg.from.trim_start_matches('!'),
        msg.message_id
    ));
    tokio::fs::write(&path, &msg.audio_data).await?;
    println!(
        "received voice from {} ({} bytes) -> {}",
        msg.from,
        msg.audio_data.len(),
        path.display()
    );
    Ok(())
}
