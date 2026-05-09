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

/// Heuristic: returns `true` if `device` looks like a serial port path
/// rather than a BLE address. Linux paths begin with `/`, Windows ports
/// with `COM`.
pub fn is_serial_path(device: &str) -> bool {
    device.starts_with('/') || device.starts_with("COM")
}

type SerialWriter = tokio::io::WriteHalf<tokio_serial::SerialStream>;
type SerialReader = tokio::io::ReadHalf<tokio_serial::SerialStream>;

/// Discover serial ports that look like Meshtastic devices.
///
/// Returns paths like `/dev/ttyUSB0`, `/dev/ttyACM0`, etc.
///
/// Only ports with an actual device behind them are returned: USB-serial
/// adapters (CP210x, CH340, FTDI, native CDC, …) are kept, while built-in
/// `Unknown` ports such as `/dev/ttyS*` on Linux — which appear even when
/// nothing is plugged in — are filtered out.
pub fn available_ports() -> Vec<PathBuf> {
    tokio_serial::available_ports()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| matches!(p.port_type, tokio_serial::SerialPortType::UsbPort(_)))
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

// Wire `SerialConnection` into the cross-transport `MeshService` plumbing.
// See the analogous impl in `crate::ble`.
#[async_trait::async_trait]
impl crate::Transport for SerialConnection {
    async fn write_to_radio(&self, bytes: &[u8]) -> Result<()> {
        SerialConnection::write_to_radio(self, bytes).await
    }
    async fn disconnect(&self) -> Result<()> {
        SerialConnection::disconnect(self).await
    }
}

/// Read one deframed protobuf payload from the serial stream.
///
/// Scans for the `START1 START2` magic, reads the 2-byte big-endian length,
/// then reads exactly that many bytes.  Returns `Ok(None)` on EOF.
async fn read_frame<R>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncRead + Unpin,
{
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

async fn read_byte<R>(reader: &mut R) -> std::io::Result<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = [0u8; 1];
    reader.read_exact(&mut buf).await?;
    Ok(buf[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncRead;

    /// Build a valid framed packet: `94 c3 <msb> <lsb> <payload...>`.
    fn frame(payload: &[u8]) -> Vec<u8> {
        let len = payload.len() as u16;
        let mut v = vec![START1, START2, (len >> 8) as u8, (len & 0xff) as u8];
        v.extend_from_slice(payload);
        v
    }

    async fn read_all<R: AsyncRead + Unpin>(r: &mut R) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(p) = read_frame(r).await.expect("read") {
            out.push(p);
        }
        out
    }

    #[tokio::test]
    async fn parses_single_frame() {
        let mut buf: &[u8] = &frame(b"hello");
        let frames = read_all(&mut buf).await;
        assert_eq!(frames, vec![b"hello".to_vec()]);
    }

    #[tokio::test]
    async fn parses_back_to_back_frames() {
        let mut data = frame(b"one");
        data.extend(frame(b"twoo"));
        data.extend(frame(b"three"));
        let mut buf: &[u8] = &data;
        let frames = read_all(&mut buf).await;
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0], b"one");
        assert_eq!(frames[1], b"twoo");
        assert_eq!(frames[2], b"three");
    }

    #[tokio::test]
    async fn skips_console_garbage_before_magic() {
        let mut data = b"some debug log line\n".to_vec();
        data.extend(frame(b"payload"));
        let mut buf: &[u8] = &data;
        let frames = read_all(&mut buf).await;
        assert_eq!(frames, vec![b"payload".to_vec()]);
    }

    #[tokio::test]
    async fn recovers_from_lone_start1() {
        // 0x94 then a non-START2 byte — the parser must keep scanning, not
        // consume the next START1 as data.
        let mut data = vec![START1, 0x00, START1, 0x42];
        data.extend(frame(b"ok"));
        let mut buf: &[u8] = &data;
        let frames = read_all(&mut buf).await;
        assert_eq!(frames, vec![b"ok".to_vec()]);
    }

    #[tokio::test]
    async fn rejects_zero_length_and_resyncs() {
        // 94 c3 00 00 (invalid) followed by a valid frame.
        let mut data = vec![START1, START2, 0x00, 0x00];
        data.extend(frame(b"after"));
        let mut buf: &[u8] = &data;
        let frames = read_all(&mut buf).await;
        assert_eq!(frames, vec![b"after".to_vec()]);
    }

    #[tokio::test]
    async fn rejects_oversized_length_and_resyncs() {
        // Length = MAX_PAYLOAD + 1 → invalid; parser resyncs.
        let bogus = (MAX_PAYLOAD + 1) as u16;
        let mut data = vec![START1, START2, (bogus >> 8) as u8, (bogus & 0xff) as u8];
        data.extend(frame(b"good"));
        let mut buf: &[u8] = &data;
        let frames = read_all(&mut buf).await;
        assert_eq!(frames, vec![b"good".to_vec()]);
    }

    #[tokio::test]
    async fn truncated_payload_is_unexpected_eof() {
        // Header advertises 10 bytes but only 3 follow.
        let mut data = vec![START1, START2, 0x00, 0x0a];
        data.extend_from_slice(b"abc");
        let mut buf: &[u8] = &data;
        let err = read_frame(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn eof_at_start1_returns_none() {
        let mut buf: &[u8] = &[START1];
        assert!(read_frame(&mut buf).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_input_returns_none() {
        let mut buf: &[u8] = &[];
        assert!(read_frame(&mut buf).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn accepts_max_payload_size() {
        let payload = vec![0xab; MAX_PAYLOAD];
        let mut buf: &[u8] = &frame(&payload);
        let frames = read_all(&mut buf).await;
        assert_eq!(frames, vec![payload]);
    }

    #[test]
    fn is_serial_path_recognises_common_forms() {
        assert!(is_serial_path("/dev/ttyUSB0"));
        assert!(is_serial_path("/dev/ttyACM1"));
        assert!(is_serial_path("COM3"));
        assert!(!is_serial_path("AA:BB:CC:DD:EE:FF"));
        assert!(!is_serial_path("aa:bb:cc:dd:ee:ff"));
    }
}
