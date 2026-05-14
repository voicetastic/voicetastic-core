//! Voicetastic CLI — text and voice over a Meshtastic mesh.

mod cli;
mod commands;
mod connect;
mod util;

use std::io::Write;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command, DeviceCmd, SettingsCmd, TextCmd, VoiceCmd};
use crate::util::{read_stdin_line, require_device};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
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
                parity,
                channel,
                to,
            } => {
                let device = require_device(cli.device)?;
                commands::voice::send(&device, channel, to, &file, bitrate, parity).await
            }
            VoiceCmd::Listen { out_dir, format } => {
                let device = require_device(cli.device)?;
                commands::voice::listen(&device, &out_dir, &format).await
            }
        },
        Command::Device { cmd } => {
            let device = require_device(cli.device)?;
            match cmd {
                DeviceCmd::Info => commands::device::info(&device).await,
                DeviceCmd::Reboot { secs } => commands::device::reboot(&device, secs).await,
                DeviceCmd::FactoryReset { yes } => {
                    commands::device::factory_reset(&device, yes).await
                }
            }
        }
        Command::Settings { cmd } => match cmd {
            SettingsCmd::List => commands::settings::list(),
            SettingsCmd::Get { key } => commands::settings::get(&key),
            SettingsCmd::Set { key, value } => commands::settings::set(&key, &value),
            SettingsCmd::Reset { key } => {
                if key.is_none() {
                    eprint!("WARNING: this will reset ALL settings to defaults. Continue? [y/N] ");
                    std::io::stderr().flush().ok();
                    let mut line = String::new();
                    if std::io::stdin().read_line(&mut line).is_ok()
                        && line.trim().eq_ignore_ascii_case("y")
                    {
                        commands::settings::reset(None)?;
                    } else {
                        eprintln!("aborted");
                    }
                } else {
                    commands::settings::reset(key.as_deref())?;
                }
                Ok(())
            }
        },
    }
}
