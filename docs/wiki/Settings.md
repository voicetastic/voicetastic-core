# Client Settings

Voicetastic Desktop keeps a small set of **client-side, persisted preferences**
behind a centralised [`SettingsApi`](../../crates/voicetastic-core/src/settings/api.rs).
The same store backs every front-end:

- **GUI** — *Settings* tab in `voicetastic-gui`.
- **CLI** — `voicetastic-cli settings {list, get, set, reset}`.
- **Android bridge** — `SettingsApi` exposed through UniFFI.

None of these values are shipped over the air. They live in a TOML file at
`$XDG_CONFIG_HOME/voicetastic/config.toml` (typically
`~/.config/voicetastic/config.toml`) on desktop, and in the app's per-app data
directory on Android.

---

## CLI usage

```bash
# Show every key, current value, default, accepted range/variants
voicetastic-cli settings list

# Read one key (machine-friendly: no trailing newline)
voicetastic-cli settings get voice.codec

# Write one key (same string format as `list` displays)
voicetastic-cli settings set voice.codec amrnb
voicetastic-cli settings set voice.amrnb_mode 7

# Reset one key, or all keys, to the default
voicetastic-cli settings reset voice.amrnb_mode
voicetastic-cli settings reset
```

`set` validates the value (range, enum membership, u8 bounds, …) and rejects
out-of-range input with a clear error.

---

## Key reference

All keys are stable string ids — they appear in `config.toml` verbatim and are
what the CLI accepts.

### `last_device`

| | |
|---|---|
| **Kind** | optional string |
| **Default** | unset |

Last BLE address (`AA:BB:CC:DD:EE:FF`) or serial port path (`/dev/ttyUSB0`)
that successfully connected. The GUI's *Devices* tab uses it for one-click
reconnect on startup. Pass an empty string to `set` to clear it.

### `voice.max_duration_secs`

| | |
|---|---|
| **Kind** | integer |
| **Range** | `1..=300` |
| **Default** | `30` |

Hard cap on a single voice-message recording. Capture stops automatically when
the cap is reached.

### `voice.reassembly_timeout_secs`

| | |
|---|---|
| **Kind** | integer |
| **Range** | `30..=900` |
| **Default** | `300` |

How long the receiver waits for missing chunks of an in-flight voice message
before emitting a partial. Long values help on slow LoRa presets (where a
full burst can take minutes); short values reclaim memory faster on busy mesh.

Applies immediately to the in-process assembler — no restart needed. The
sender's retransmit retain TTL is coupled to this same value so a NACK can't
arrive for a message the sender already forgot.

### `voice.codec`

| | |
|---|---|
| **Kind** | enum |
| **Variants** | `amrnb`, `codec2`, `opus` |
| **Default** | `amrnb` |

Codec used to encode new outgoing voice messages. Inbound messages are always
decoded using the codec advertised in their header, so this setting only
affects what *you* send.

| Codec    | Rate    | Bitrates                              | Notes                                  |
|----------|---------|---------------------------------------|----------------------------------------|
| `amrnb`  | 8 kHz   | 4.75 – 12.2 kbps (8 modes)            | Wire-compatible with Voicetastic Android. |
| `codec2` | 8 kHz   | 1.2 – 3.2 kbps (6 modes)              | Most LoRa-friendly bitrates.           |
| `opus`   | 48 kHz  | 12 kbps (Application::Voip)           | Wideband; larger payloads.             |

### `voice.amrnb_mode`

| | |
|---|---|
| **Kind** | integer |
| **Range** | `0..=7` |
| **Default** | `7` (MR122, 12.20 kbps) |

AMR-NB bitrate mode used when `voice.codec = amrnb`.

| Value | Mode  | Bitrate    | Bytes / 20 ms frame (incl. ToC) |
|-------|-------|------------|---------------------------------|
| `0`   | MR475 | 4.75 kbps  | 13                              |
| `1`   | MR515 | 5.15 kbps  | 14                              |
| `2`   | MR590 | 5.90 kbps  | 16                              |
| `3`   | MR670 | 6.70 kbps  | 18                              |
| `4`   | MR740 | 7.40 kbps  | 20                              |
| `5`   | MR795 | 7.95 kbps  | 21                              |
| `6`   | MR102 | 10.20 kbps | 27                              |
| `7`   | MR122 | 12.20 kbps | 32                              |

Lower values are friendlier to slow LoRa presets at the cost of audio quality.

### `voice.codec2_mode`

| | |
|---|---|
| **Kind** | integer |
| **Range** | `0..=5` |
| **Default** | `5` (1200 bps) |

Codec2 bitrate mode used when `voice.codec = codec2`.

| Value | Bitrate   |
|-------|-----------|
| `0`   | 3200 bps  |
| `1`   | 2400 bps  |
| `2`   | 1600 bps  |
| `3`   | 1400 bps  |
| `4`   | 1300 bps  |
| `5`   | 1200 bps  |

At 1200 bps a 30 s clip fits in ~4.5 kB — recommended for `LongFast` and
slower presets.

---

## File format

`config.toml` is a flat TOML document; unset keys are omitted so the file
stays small and human-editable:

```toml
last_device = "AA:BB:CC:DD:EE:FF"
voice_codec = "amrnb"
voice_amrnb_mode = 7
max_voice_duration_secs = 30
reassembly_timeout_secs = 300
```

The field names on disk use snake_case; the dotted keys (`voice.codec`,
`voice.amrnb_mode`, …) are the stable wire ids used by the CLI and the
Android bridge, and are translated to/from the TOML schema by
[`SettingsApi`](../../crates/voicetastic-core/src/settings/api.rs).

If the file is missing, malformed, or contains an unknown value, the API
silently falls back to defaults instead of refusing to start.
