//! `voicetastic voice {send,listen}` — voice-message commands.
//!
//! The CLI uses the codec-agnostic voice protocol with codec=AMR-NB.
//! `send` strips the `#!AMR\n` file header before chunking; `listen`
//! re-prepends it before writing each received message back to disk.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use voicetastic_core::MeshtasticService;
use voicetastic_core::ports::PRIVATE_APP;
use voicetastic_core::voice::{
    AssemblerConfig, AssemblyEvent, PROTOCOL_VERSION, SendRequest, SendStatus, VoiceAssembler,
    VoiceCodec, VoiceDestination, VoiceMessage, VoiceSender, detect_version,
};

use crate::connect::connect;
use crate::util::disconnect_with_timeout;

/// AMR-NB file header — stripped on send, re-prepended on receive.
const AMR_FILE_HEADER: &[u8] = b"#!AMR\n";

/// File extension for each codec.
fn codec_extension(codec: VoiceCodec) -> &'static str {
    match codec {
        VoiceCodec::AmrNb => "amr",
        VoiceCodec::Opus => "opus",
        VoiceCodec::Codec2 => "c2",
        VoiceCodec::PcmS16Le => "pcm",
        VoiceCodec::Unknown(_) => "bin",
    }
}

pub async fn send(
    device: &str,
    channel: u32,
    to: Option<u32>,
    file: &Path,
    bitrate: u8,
    parity: u8,
) -> Result<()> {
    if bitrate > 7 {
        bail!("--bitrate must be 0..=7 (AMR-NB ordinal)");
    }
    let bytes = tokio::fs::read(file)
        .await
        .with_context(|| format!("reading {}", file.display()))?;
    // Strip optional AMR file header — the protocol carries raw codec bytes.
    let audio = bytes
        .strip_prefix(AMR_FILE_HEADER)
        .unwrap_or(bytes.as_slice());
    if audio.is_empty() {
        bail!("file {} contains no audio frames", file.display());
    }

    let svc = MeshtasticService::new().await?;
    connect(&svc, device).await?;

    // `VoiceSender` owns build → register → burst → NACK-driven
    // retransmit → linger; the CLI just consumes the status stream.
    let sender = VoiceSender::new(svc.clone());
    let handle = sender
        .send(SendRequest {
            audio: audio.to_vec(),
            codec: VoiceCodec::AmrNb,
            codec_param: bitrate,
            channel,
            to,
            parity_count: parity,
            last_in_stream: true,
            ..Default::default()
        })
        .context("starting voice send")?;
    info!(message_id = handle.message_id, "sending voice");
    println!("sending voice message_id={}", handle.message_id);

    let mut rx = handle.subscribe();
    loop {
        match rx.recv().await {
            Ok(status) => {
                let terminal = status.is_terminal();
                match &status {
                    SendStatus::Building {
                        total_data,
                        parity_count,
                        ..
                    } => {
                        println!("  building: {total_data} data + {parity_count} parity frames");
                    }
                    SendStatus::Sending { sent, total, .. } => {
                        info!(sent, total, "voice frame enqueued");
                    }
                    SendStatus::BurstComplete { packet_ids, .. } => {
                        println!("  burst complete ({} frames on the wire)", packet_ids.len());
                    }
                    SendStatus::Retransmitting { chunks, .. } => {
                        println!("  retransmitting {} chunk(s)", chunks.len());
                    }
                    SendStatus::Complete { message_id } => {
                        println!("  complete (message_id={message_id})");
                    }
                    SendStatus::GaveUp { message_id } => {
                        println!("  receiver gave up (message_id={message_id})");
                    }
                    SendStatus::Failed { message, .. } => {
                        println!("  failed: {message}");
                    }
                }
                if terminal {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!(skipped = n, "CLI status subscriber lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }

    disconnect_with_timeout(&svc).await;
    Ok(())
}

pub async fn listen(device: &str, out_dir: &Path, format: &str) -> Result<()> {
    if format != "amr" {
        warn!(
            "--format {} is not yet implemented; output uses the incoming codec's native extension",
            format
        );
    }
    tokio::fs::create_dir_all(out_dir)
        .await
        .with_context(|| format!("creating output directory {}", out_dir.display()))?;
    // Canonicalize once so symlinks / `..` segments in the user-provided path
    // are resolved up front. Subsequent writes are validated against this
    // base to prevent any future filename change from escaping it.
    let base_dir = tokio::fs::canonicalize(out_dir)
        .await
        .with_context(|| format!("resolving --out-dir {}", out_dir.display()))?;
    if !base_dir.is_dir() {
        bail!("--out-dir {} is not a directory", base_dir.display());
    }
    let svc = MeshtasticService::new().await?;
    connect(&svc, device).await?;
    let assembler = VoiceAssembler::new({
        let mut cfg = AssemblerConfig::default();
        cfg.sync_nack_cap_to_timeout();
        cfg
    });
    let mut rx = svc.subscribe_data();
    info!("listening for voice messages, ctrl-c to stop");
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tick.tick() => {
                let out = assembler.tick();
                for completed in out.finalized {
                    save_voice(&base_dir, &completed).await?;
                }
                // Forward outbound NACKs to the originating sender so the
                // receive→send selective-retransmission loop can close.
                for nack in out.nacks {
                    let to_node = match voicetastic_core::ids::node_id_to_num(&nack.from) {
                        Ok(n) => n,
                        Err(e) => {
                            warn!(from = %nack.from, ?e, "skip NACK: bad node id");
                            continue;
                        }
                    };
                    if let Err(e) = svc
                        .send_data(
                            PRIVATE_APP as i32,
                            nack.frame,
                            nack.channel,
                            Some(to_node),
                            false,
                            false, // want_response
                        )
                        .await
                    {
                        warn!(?e, "failed to transmit voice NACK");
                    }
                }
            }
            data = rx.recv() => match data {
                Ok(d) => {
                    if d.portnum != PRIVATE_APP as i32 { continue; }
                    if detect_version(&d.payload) != Some(PROTOCOL_VERSION) { continue; }
                    let from_id = voicetastic_core::ids::node_num_to_id(d.from);
                    let to = if d.to == voicetastic_core::ports::BROADCAST_ADDR {
                        VoiceDestination::Broadcast
                    } else {
                        VoiceDestination::Node(voicetastic_core::node::NodeId::from_u32(d.to))
                    };
                    match assembler.accept(&from_id, to, d.channel, &d.payload) {
                        AssemblyEvent::Complete(msg) => save_voice(&base_dir, &msg).await?,
                        AssemblyEvent::Pending { .. } | AssemblyEvent::Duplicate => {}
                        AssemblyEvent::Nack(_) => {}
                        AssemblyEvent::Rejected(e) => warn!(?e, "rejected voice frame"),
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "voice listener lagged, dropped chunks");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    disconnect_with_timeout(&svc).await;
    Ok(())
}

/// Build a sanitized output path under `base_dir`. Returns an error if the
/// constructed filename contains path separators or otherwise resolves
/// outside `base_dir` (defense-in-depth — the filename comes from a `u32`
/// node number formatted as `!aabbccdd` plus a `u32` message id, so this
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

async fn save_voice(base_dir: &Path, msg: &VoiceMessage) -> Result<()> {
    let ext = codec_extension(msg.codec);
    let filename = format!(
        "{}_{}.{ext}",
        msg.from.trim_start_matches('!'),
        msg.message_id,
    );
    let path = safe_join(base_dir, &filename)?;
    let out = if msg.codec == VoiceCodec::AmrNb {
        // Re-prepend the AMR file header so the resulting file is playable.
        let mut buf = Vec::with_capacity(AMR_FILE_HEADER.len() + msg.audio.len());
        buf.extend_from_slice(AMR_FILE_HEADER);
        buf.extend_from_slice(&msg.audio);
        buf
    } else {
        msg.audio.clone()
    };
    tokio::fs::write(&path, &out).await?;
    println!(
        "received voice from {} ({} bytes, complete={}, codec={:?}) -> {}",
        msg.from,
        out.len(),
        msg.is_complete,
        msg.codec,
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
