//! Voicetastic CLI — text and voice over a Meshtastic mesh.

mod cli;
mod commands;
mod connect;
mod util;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use voicetastic_core::voice::AmrNbBitrate;

use crate::cli::{Cli, Command, TextCmd, VoiceCmd};
use crate::util::{read_stdin_line, require_device};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_env_filter(EnvFilter::try_new(&cli.log).unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    match cli.command {
        Command::Scan { seconds } => commands::scan::run(seconds).await,
        Command::Text { cmd } => match cmd {
            TextCmd::Send { channel, to, text } => {
                let device = require_device(cli.device)?;
                let body = match text {
                    Some(t) => t,
                    None => read_stdin_line().await?,
                };
                commands::text::send(&device, channel, to, &body).await
            }
            TextCmd::Listen => {
                let device = require_device(cli.device)?;
                commands::text::listen(&device).await
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
                let bitrate =
                    AmrNbBitrate::from_ordinal(bitrate).context("--bitrate must be 0..=7")?;
                commands::voice::send(&device, channel, to, &file, bitrate).await
            }
            VoiceCmd::Listen { out_dir } => {
                let device = require_device(cli.device)?;
                commands::voice::listen(&device, &out_dir).await
            }
        },
    }
}
