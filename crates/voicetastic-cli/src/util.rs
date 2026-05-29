//! Small CLI helpers shared across commands.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::warn;
use voicetastic_core::MeshtasticService;

pub use voicetastic_core::serial::is_serial_path as is_serial;

/// Time-bounded disconnect used by listener commands on Ctrl-C. A hung
/// transport (e.g. a serial port whose USB endpoint stalled) can make
/// `disconnect()` block on the per-write timeout (~20 s) — long enough
/// that users assume the process has wedged and reach for Ctrl-C again.
/// Drop the result either way; the process is exiting.
const DISCONNECT_DEADLINE: Duration = Duration::from_secs(2);

pub async fn disconnect_with_timeout(svc: &MeshtasticService) {
    match tokio::time::timeout(DISCONNECT_DEADLINE, svc.disconnect()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!(?e, "disconnect failed"),
        Err(_) => warn!(
            timeout_s = DISCONNECT_DEADLINE.as_secs(),
            "disconnect timed out; exiting anyway",
        ),
    }
}

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
    let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!(
            "empty message body (stdin EOF or blank line); pass --message or pipe non-empty text"
        ));
    }
    Ok(trimmed)
}
