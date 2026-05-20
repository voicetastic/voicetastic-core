# Voicetastic Desktop

Linux desktop companion for [Voicetastic](https://git.cha-sam.re/acarteron/voicetastic) (Android).
Communicates with Meshtastic radios over BLE or USB serial, providing text
messaging and live voice message exchange (AMR-NB / Codec2 / Opus) — wire-compatible
with the Android app for AMR-NB.

## Workspace Layout

| Crate | Description |
|---|---|
| `voicetastic-core` | Shared library: BLE + serial transport, Meshtastic protobuf codec, voice chunker/assembler, `MeshService` façade |
| `voicetastic-cli` | CLI (`clap`): `scan`, `text send/listen`, `voice send/listen`, `device reboot/factory-reset` |
| `voicetastic-gui` | GUI (`eframe`/`egui`): three-tab app (Devices, Chat, Settings) |
| `voicetastic-android-bridge` | UniFFI bindings exposing core functionality to the Android app |

## Documentation

- **Voice protocol spec** (normative wire format): [Voice-Protocol wiki page](https://git.cha-sam.re/voicetastic/voicetastic-desktop/-/wikis/Voice-Protocol)
- **Voice protocol wiki** (implementer guide, examples, diagrams): [project wiki](https://git.cha-sam.re/voicetastic/voicetastic-desktop/-/wikis/Home)

## Prerequisites

- **Rust 1.95+** (edition 2024 workspace)
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

Voice messages are captured live from the microphone, encoded, and protected with
FEC (Reed-Solomon), then chunked into ≤ 215-byte Meshtastic data packets sent
over `PortNum::PRIVATE_APP` with a **16-byte v3 header** (protocol version `0x03`),
see the [Voice-Protocol wiki page](https://git.cha-sam.re/voicetastic/voicetastic-desktop/-/wikis/Voice-Protocol). Four codecs are supported:

| Codec     | Wire id | Rate    | Bitrates                          | Notes                                  |
|-----------|---------|---------|-----------------------------------|----------------------------------------|
| AMR-NB    | `0`     | 8 kHz   | 4.75 – 12.2 kbps (8 modes)        | Default. Wire-compatible with Android. |
| Opus      | `1`     | 48 kHz  | 6 – 64 kbps (configurable)        | Wideband; larger payloads.             |
| PCM_S16LE | `2`     | 48 kHz  | 768 kbps                          | Raw PCM; not recommended for LoRa.     |
| Codec2    | `3`     | 8 kHz   | 1.2 – 3.2 kbps (6 modes)          | Most LoRa-friendly bitrates.           |

The codec used to encode a message is advertised in its header, so peers always
decode using the correct codec regardless of their own outgoing-codec setting.

See the [Settings wiki page](https://git.cha-sam.re/voicetastic/voicetastic-desktop/-/wikis/Settings) for the client-side settings
(codec choice, bitrate, recording duration, reassembly timeout) and
the [Voice-Protocol wiki page](https://git.cha-sam.re/voicetastic/voicetastic-desktop/-/wikis/Voice-Protocol) for the wire format.

## Settings

Persisted client settings live under `$XDG_CONFIG_HOME/voicetastic/config.toml`
(`~/.config/voicetastic/config.toml`). The same file backs the GUI's *Settings*
tab, the CLI's `settings` subcommand, and the Android bridge.

```bash
# List every known setting, current value, default, and accepted range
cargo run -p voicetastic-cli -- settings list

# Read or write a single key
cargo run -p voicetastic-cli -- settings get voice.codec
cargo run -p voicetastic-cli -- settings set voice.codec amrnb
cargo run -p voicetastic-cli -- settings set voice.amrnb_mode 7

# Restore one key (or every key) to its default
cargo run -p voicetastic-cli -- settings reset voice.amrnb_mode
cargo run -p voicetastic-cli -- settings reset
```

Full key reference: [Settings wiki page](https://git.cha-sam.re/voicetastic/voicetastic-desktop/-/wikis/Settings).

## License

GPL-3.0-or-later. See [LICENSE](LICENSE) for the full text.
