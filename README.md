# Voicetastic Desktop

Linux desktop companion for [Voicetastic](https://github.com/nicogig/Voicetastic) (Android).
Communicates with Meshtastic radios over BLE or USB serial, providing text
messaging and AMR-NB voice message exchange — wire-compatible with the Android app.

## Workspace Layout

| Crate | Description |
|---|---|
| `voicetastic-core` | Shared library: BLE + serial transport, Meshtastic protobuf codec, voice chunker/assembler, `MeshService` façade |
| `voicetastic-cli` | CLI (`clap`): `scan`, `text send/listen`, `voice send/listen`, `device reboot/factory-reset` |
| `voicetastic-gui` | GUI (`eframe`/`egui`): three-tab app (Devices, Chat, Settings) |

## Prerequisites

- **Rust 1.80+** (edition 2024 workspace)
- **Linux** with BlueZ (D-Bus BLE stack)
- **protoc** (Protocol Buffers compiler)

```bash
# Arch
sudo pacman -S bluez bluez-utils protobuf

# Debian / Ubuntu
sudo apt install bluez libdbus-1-dev protobuf-compiler
```

### BLE permissions

Either run as root, or grant your user the `net_admin` capability / add to the
`bluetooth` group, then ensure the BlueZ D-Bus policy allows access:

```bash
sudo usermod -aG bluetooth $USER
# or, per-binary:
sudo setcap cap_net_admin+ep target/debug/voicetastic-cli
```

## Build

Protobuf definitions are pulled from the upstream
[meshtastic/protobufs](https://github.com/meshtastic/protobufs) repo via a git
submodule. Make sure to initialise it:

```bash
git clone --recurse-submodules https://github.com/<you>/voicetastic-desktop.git
# or, if already cloned:
git submodule update --init
```

Then build:

```bash
cargo build --workspace
```

## Run

### CLI

The `--device` flag accepts either a BLE address (`AA:BB:CC:DD:EE:FF`) or a
serial port path (`/dev/ttyUSB0`, `/dev/ttyACM0`).

```bash
# Scan for nearby Meshtastic devices (BLE + serial ports)
cargo run -p voicetastic-cli -- scan

# Connect via BLE
cargo run -p voicetastic-cli -- --device AA:BB:CC:DD:EE:FF text send --message "Hello mesh!"

# Connect via USB serial
cargo run -p voicetastic-cli -- --device /dev/ttyUSB0 text send --message "Hello mesh!"

# Listen for incoming texts
cargo run -p voicetastic-cli -- --device AA:BB:CC:DD:EE:FF text listen

# Send a voice message (.amr file)
cargo run -p voicetastic-cli -- --device AA:BB:CC:DD:EE:FF voice send --file msg.amr

# Listen and save incoming voice messages
cargo run -p voicetastic-cli -- --device AA:BB:CC:DD:EE:FF voice listen --out-dir ./received
```

### GUI

```bash
cargo run -p voicetastic-gui
```

## Test

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Voice Protocol

Voice messages use AMR-NB (Adaptive Multi-Rate Narrowband) codec at 8 kHz.
Audio is chunked into ≤ 200-byte Meshtastic data packets sent over
`PortNum::PRIVATE_APP` with a 4-byte header (`msgId · chunkIndex · totalChunks · bitrateIndex`).
The desktop app reads/writes standard `.amr` files — no live microphone support
in this version.

## License

MIT
