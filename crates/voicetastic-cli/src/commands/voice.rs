//! `voicetastic voice {send,listen}` — voice-message commands (raw AMR I/O).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use voicetastic_core::ports::PRIVATE_APP;
use voicetastic_core::service::MeshService;
use voicetastic_core::voice::{
    AmrNbBitrate, AssemblyEvent, VoiceAssembler, VoiceChunk, VoiceChunker, VoiceConfig,
    VoiceMessage, random_message_id,
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
    let message_id = random_message_id();
    let chunks = VoiceChunker::chunk(&bytes, message_id, bitrate)?;
    info!(chunks = chunks.len(), "sending voice");
    let ids = svc.send_voice_chunks(chunks, channel, to).await?;
    println!("sent voice message_id={message_id}, packet_ids={:?}", ids);
    let _ = svc.disconnect().await;
    Ok(())
}

pub async fn listen(device: &str, out_dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(out_dir).await.ok();
    // Canonicalize once so symlinks / `..` segments in the user-provided path
    // are resolved up front. Subsequent writes are validated against this
    // base to prevent any future filename change from escaping it.
    let base_dir = tokio::fs::canonicalize(out_dir)
        .await
        .with_context(|| format!("resolving --out-dir {}", out_dir.display()))?;
    if !base_dir.is_dir() {
        bail!("--out-dir {} is not a directory", base_dir.display());
    }
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
                    save_amr(&base_dir, &completed).await?;
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
                        AssemblyEvent::Complete(msg) => save_amr(&base_dir, &msg).await?,
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

/// Build a sanitized output path under `base_dir`. Returns an error if the
/// constructed filename contains path separators or otherwise resolves
/// outside `base_dir` (defense-in-depth — the filename comes from a `u32`
/// node number formatted as `!aabbccdd` plus a `u16` message id, so this
/// should never fail in practice).
fn safe_join(base_dir: &Path, filename: &str) -> Result<PathBuf> {
    if filename.is_empty()
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains('\0')
        || filename == "."
        || filename == ".."
    {
        bail!("refusing unsafe voice filename: {filename:?}");
    }
    let path = base_dir.join(filename);
    if !path.starts_with(base_dir) {
        bail!(
            "voice file path {} would escape {}",
            path.display(),
            base_dir.display()
        );
    }
    Ok(path)
}

async fn save_amr(base_dir: &Path, msg: &VoiceMessage) -> Result<()> {
    let filename = format!(
        "{}_{}.amr",
        msg.from.trim_start_matches('!'),
        msg.message_id
    );
    let path = safe_join(base_dir, &filename)?;
    tokio::fs::write(&path, &msg.audio_data).await?;
    println!(
        "received voice from {} ({} bytes) -> {}",
        msg.from,
        msg.audio_data.len(),
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::safe_join;
    use std::path::Path;

    #[test]
    fn safe_join_accepts_simple_filenames() {
        let base = Path::new("/tmp");
        let p = safe_join(base, "a1b2c3d4_42.amr").unwrap();
        assert_eq!(p, Path::new("/tmp/a1b2c3d4_42.amr"));
    }

    #[test]
    fn safe_join_rejects_traversal() {
        let base = Path::new("/tmp");
        for bad in ["..", ".", "", "../etc", "a/b", "a\\b", "a\0b"] {
            assert!(
                safe_join(base, bad).is_err(),
                "should reject filename {bad:?}"
            );
        }
    }
}
