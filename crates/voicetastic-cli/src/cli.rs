//! Command-line argument definitions.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "voicetastic",
    version,
    about = "Voicetastic — Meshtastic text + voice CLI"
)]
pub struct Cli {
    /// BLE address (`AA:BB:CC:DD:EE:FF`) or serial port path (`/dev/ttyUSB0`).
    #[arg(long, global = true)]
    pub device: Option<String>,

    /// Log level filter (e.g. info, debug, voicetastic_core=debug).
    #[arg(long, global = true, default_value = "info")]
    pub log: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
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
    /// Device commands (info, reboot, factory reset).
    Device {
        #[command(subcommand)]
        cmd: DeviceCmd,
    },
}

#[derive(Debug, Subcommand)]
pub enum TextCmd {
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
pub enum VoiceCmd {
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

#[derive(Debug, Subcommand)]
pub enum DeviceCmd {
    /// Print local node info, firmware metadata, and known nodes.
    Info,
    /// Schedule a reboot.
    Reboot {
        /// Seconds until reboot.
        #[arg(long, default_value_t = 5)]
        secs: i32,
    },
    /// Factory-reset the device's configuration (preserves BLE bonds).
    FactoryReset {
        /// Required confirmation flag.
        #[arg(long)]
        yes: bool,
    },
}
