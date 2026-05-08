//! Small CLI helpers shared across commands.

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};

pub use voicetastic_core::serial::is_serial_path as is_serial;

pub fn require_device(d: Option<String>) -> Result<String> {
    d.context(
        "missing --device <BLE address or serial port>; run `voicetastic scan` to discover one",
    )
}

pub async fn read_stdin_line() -> Result<String> {
    let mut line = String::new();
    BufReader::new(tokio::io::stdin())
        .read_line(&mut line)
        .await?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}
