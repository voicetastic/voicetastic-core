//! Voicetastic CLI — text and voice over a Meshtastic mesh.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use voicetastic_core::ble::DiscoveredDevice;
use voicetastic_core::ports::PRIVATE_APP;
use voicetastic_core::service::{ConnectionState, MeshService};
use voicetastic_core::voice::{
    AmrNbBitrate, AssemblyEvent, VoiceAssembler, VoiceChunk, VoiceChunker, VoiceConfig,
};

#[derive(Debug, Parser)]
#[command(name = "voicetastic", version, about = "Voicetastic — Meshtastic text + voice CLI")]
struct Cli {
    /// BLE address (`AA:BB:CC:DD:EE:FF`) or serial port path (`/dev/ttyUSB0`).
    #[arg(long, global = true)]
    device: Option<String>,

    /// Log level filter (e.g. info, debug, voicetastic_core=debug).
    #[arg(long, global = true, default_value = "info")]
    log: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Scan for nearby Meshtastic devices.
    Scan {
        /// Scan duration (seconds).
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
    /// Text-message commands.
    Text {
        #[command(subcommand)]
        cmd: TextCmd,
    },
    /// Voice-message commands (raw `.amr` file I/O).
    Voice {
        #[command(subcommand)]
        cmd: VoiceCmd,
    },
}

#[derive(Debug, Subcommand)]
enum TextCmd {
    /// Send a text message on the primary channel.
    Send {
        /// Channel index (0 = primary).
        #[arg(long, default_value_t = 0)]
        channel: u32,
        /// Direct-message destination node number (decimal). Default: broadcast.
        #[arg(long)]
        to: Option<u32>,
        /// Message body. If omitted, read one line from stdin.
        text: Option<String>,
    },
    /// Listen for incoming text messages until Ctrl-C.
    Listen,
}

#[derive(Debug, Subcommand)]
enum VoiceCmd {
    /// Send an `.amr` file as a chunked voice message.
    Send {
        /// Path to an existing AMR-NB file (must start with `#!AMR\n`).
        file: PathBuf,
        /// AMR-NB bitrate ordinal (0..=7). Should match the file's frames.
        #[arg(long, default_value_t = 5)]
        bitrate: u8,
        /// Channel index (0 = primary).
        #[arg(long, default_value_t = 0)]
        channel: u32,
        /// Direct-message destination node number. Default: broadcast.
        #[arg(long)]
        to: Option<u32>,
    },
    /// Listen for incoming voice messages and write each as a `.amr` file.
    Listen {
        /// Output directory.
        #[arg(long, default_value = ".")]
        out_dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_new(&cli.log).unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match cli.command {
        Command::Scan { seconds } => run_scan(seconds).await,
        Command::Text { cmd } => match cmd {
            TextCmd::Send { channel, to, text } => {
                let device = require_device(cli.device)?;
                let body = match text {
                    Some(t) => t,
                    None => read_stdin_line().await?,
                };
                run_text_send(&device, channel, to, &body).await
            }
            TextCmd::Listen => {
                let device = require_device(cli.device)?;
                run_text_listen(&device).await
            }
        },
        Command::Voice { cmd } => match cmd {
            VoiceCmd::Send {
                file,
                bitrate,
                channel,
                to,
            } => {
                let device = require_device(cli.device)?;
                let bitrate = AmrNbBitrate::from_ordinal(bitrate)
                    .context("--bitrate must be 0..=7")?;
                run_voice_send(&device, channel, to, &file, bitrate).await
            }
            VoiceCmd::Listen { out_dir } => {
                let device = require_device(cli.device)?;
                run_voice_listen(&device, &out_dir).await
            }
        },
    }
}

fn require_device(d: Option<String>) -> Result<String> {
    d.context("missing --device <BLE address or serial port>; run `voicetastic scan` to discover one")
}

/// Returns `true` if the device string looks like a serial port path rather
/// than a BLE address.
fn is_serial(device: &str) -> bool {
    device.starts_with('/') || device.starts_with("COM")
}

async fn read_stdin_line() -> Result<String> {
    let mut line = String::new();
    BufReader::new(tokio::io::stdin())
        .read_line(&mut line)
        .await?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

async fn run_scan(seconds: u64) -> Result<()> {
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

async fn connect(svc: &MeshService, device: &str) -> Result<()> {
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

async fn run_text_send(device: &str, channel: u32, to: Option<u32>, body: &str) -> Result<()> {
    let svc = MeshService::new().await?;
    connect(&svc, device).await?;
    let id = svc.send_text(body, channel, to).await?;
    println!("sent text id={id}");
    let _ = svc.disconnect().await;
    Ok(())
}

async fn run_text_listen(device: &str) -> Result<()> {
    let svc = MeshService::new().await?;
    connect(&svc, device).await?;
    let mut rx = svc.subscribe_text();
    info!("listening for text messages, ctrl-c to stop");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            msg = rx.recv() => match msg {
                Ok(t) => println!("[ch{} {} -> {}] {}", t.channel, t.from_id, t.to, t.text),
                Err(_) => break,
            },
        }
    }
    let _ = svc.disconnect().await;
    Ok(())
}

async fn run_voice_send(
    device: &str,
    channel: u32,
    to: Option<u32>,
    file: &std::path::Path,
    bitrate: AmrNbBitrate,
) -> Result<()> {
    let bytes = tokio::fs::read(file)
        .await
        .with_context(|| format!("reading {}", file.display()))?;
    let svc = MeshService::new().await?;
    connect(&svc, device).await?;
    let message_id: u16 = (chrono::Utc::now().timestamp_millis() as u16).max(1);
    let chunks = VoiceChunker::chunk(&bytes, message_id, bitrate)?;
    info!(chunks = chunks.len(), "sending voice");
    let ids = svc.send_voice_chunks(chunks, channel, to).await?;
    println!("sent voice message_id={message_id}, packet_ids={:?}", ids);
    let _ = svc.disconnect().await;
    Ok(())
}

async fn run_voice_listen(device: &str, out_dir: &std::path::Path) -> Result<()> {
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
                Err(_) => break,
            },
        }
    }
    let _ = svc.disconnect().await;
    Ok(())
}

async fn save_amr(
    out_dir: &std::path::Path,
    msg: &voicetastic_core::voice::VoiceMessage,
) -> Result<()> {
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
