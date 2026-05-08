//! Serial/USB transport for Meshtastic nodes.
//!
//! Meshtastic devices expose a streaming protobuf interface over USB-serial
//! (typically `/dev/ttyUSB0` or `/dev/ttyACM0`).  Each packet is framed with
//! a 4-byte header:
//!
//! | Byte | Value        |
//! |------|--------------|
//! | 0    | `0x94`       |
//! | 1    | `0xc3`       |
//! | 2    | length MSB   |
//! | 3    | length LSB   |
//!
//! followed by the protobuf payload (≤ 512 bytes).

use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc, watch};
use tokio_serial::SerialPortBuilderExt;
use tracing::{debug, warn};

use crate::error::{Error, Result};

/// Magic bytes that begin every serial-framed packet.
const START1: u8 = 0x94;
const START2: u8 = 0xc3;
/// Maximum protobuf payload length accepted by the device.
const MAX_PAYLOAD: usize = 512;
/// Default baud rate for Meshtastic USB-serial devices.
pub const DEFAULT_BAUD: u32 = 115_200;

type SerialWriter = tokio::io::WriteHalf<tokio_serial::SerialStream>;
type SerialReader = tokio::io::ReadHalf<tokio_serial::SerialStream>;

/// Discover serial ports that look like Meshtastic devices.
///
/// Returns paths like `/dev/ttyUSB0`, `/dev/ttyACM0`, etc.
pub fn available_ports() -> Vec<PathBuf> {
    tokio_serial::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| PathBuf::from(p.port_name))
        .collect()
}

/// An open Meshtastic serial connection.
///
/// Provides the same logical interface as [`crate::ble::Connection`]:
/// `write_to_radio`, `subscribe_inbound`, and `disconnect`.
pub struct SerialConnection {
    writer: Mutex<SerialWriter>,
    reader: Mutex<Option<SerialReader>>,
    port_path: PathBuf,
    shutdown: watch::Sender<bool>,
}

impl SerialConnection {
    /// Open a serial port at the given path and baud rate.
    pub async fn open(path: impl AsRef<Path>, baud: u32) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let port = tokio_serial::new(path.to_string_lossy(), baud)
            .open_native_async()
            .map_err(|e| Error::Other(format!("serial open {}: {e}", path.display())))?;

        let (reader, writer) = tokio::io::split(port);
        let (shutdown_tx, _) = watch::channel(false);

        Ok(Self {
            writer: Mutex::new(writer),
            reader: Mutex::new(Some(reader)),
            port_path: path,
            shutdown: shutdown_tx,
        })
    }

    /// Write a `ToRadio` protobuf payload, wrapped in the 4-byte serial header.
    pub async fn write_to_radio(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_PAYLOAD {
            return Err(Error::Other(format!(
                "payload too large: {} > {MAX_PAYLOAD}",
                bytes.len()
            )));
        }
        let len = bytes.len() as u16;
        let header = [START1, START2, (len >> 8) as u8, (len & 0xFF) as u8];
        let mut w = self.writer.lock().await;
        w.write_all(&header).await?;
        w.write_all(bytes).await?;
        w.flush().await?;
        debug!(len = bytes.len(), "serial write_to_radio");
        Ok(())
    }

    /// Subscribe to inbound `FromRadio` payloads.
    ///
    /// Spawns a background task that reads and deframes serial packets.
    /// Can only be called once per connection (takes ownership of the reader).
    pub async fn subscribe_inbound(&self) -> Result<mpsc::Receiver<Vec<u8>>> {
        let reader = self
            .reader
            .lock()
            .await
            .take()
            .ok_or_else(|| Error::Other("subscribe_inbound already called".into()))?;

        let (tx, rx) = mpsc::channel(64);
        let mut shutdown_rx = self.shutdown.subscribe();

        tokio::spawn(async move {
            let mut reader = reader;
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => break,
                    result = read_frame(&mut reader) => {
                        match result {
                            Ok(Some(payload)) => {
                                if tx.send(payload).await.is_err() {
                                    break;
                                }
                            }
                            Ok(None) => break, // EOF
                            Err(e) => {
                                warn!(?e, "serial read error");
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(rx)
    }

    /// Close the serial connection.
    pub async fn disconnect(&self) -> Result<()> {
        let _ = self.shutdown.send(true);
        Ok(())
    }

    /// The path of the serial port, e.g. `/dev/ttyUSB0`.
    pub fn port_path(&self) -> &Path {
        &self.port_path
    }
}

/// Read one deframed protobuf payload from the serial stream.
///
/// Scans for the `START1 START2` magic, reads the 2-byte big-endian length,
/// then reads exactly that many bytes.  Returns `Ok(None)` on EOF.
async fn read_frame(reader: &mut SerialReader) -> std::io::Result<Option<Vec<u8>>> {
    loop {
        let b = match read_byte(reader).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        };
        if b != START1 {
            continue; // debug console output — skip
        }
        let b2 = match read_byte(reader).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        };
        if b2 != START2 {
            continue; // false positive on START1
        }
        // Read length (big-endian u16)
        let msb = read_byte(reader).await?;
        let lsb = read_byte(reader).await?;
        let len = ((msb as usize) << 8) | (lsb as usize);
        if len == 0 || len > MAX_PAYLOAD {
            warn!(len, "serial: invalid payload length, re-syncing");
            continue;
        }
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).await?;
        return Ok(Some(payload));
    }
}

async fn read_byte(reader: &mut SerialReader) -> std::io::Result<u8> {
    let mut buf = [0u8; 1];
    reader.read_exact(&mut buf).await?;
    Ok(buf[0])
}
