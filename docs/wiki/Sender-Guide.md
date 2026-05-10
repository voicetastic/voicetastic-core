# Sender Guide

[← Home](Home.md)

How to build a compatible Voicetastic transmitter — protocol-level
checklist plus reference-implementation pointers.

---

## Inputs

You need:

- **Audio bytes** — pre-encoded codec frames, **without container header**
  (e.g. for AMR-NB, strip the leading `#!AMR\n`).
- **Channel index** — the Meshtastic channel you'll transmit on.
- **Destination** — `Some(node_num)` for a DM (sets `want_ack=true`) or
  `None` for broadcast.
- **Modem preset** — read from `Config.LoRaConfig.modem_preset` to derive
  pacing and a default `chunk_size`.
- **(Optional) Channel PSK** — to enable the AES-GCM envelope.

---

## Step 1 — Pick parameters

| Parameter       | Source                                                                                    |
|-----------------|-------------------------------------------------------------------------------------------|
| `message_id`    | `random_message_id()` — non-zero `u32` from OS RNG.                                       |
| `stream_seq`    | Per-`(from, channel)` monotonic counter; wrap at 256.                                     |
| `codec`         | Whatever you encoded with.                                                                |
| `codec_param`   | Codec-specific (AMR-NB ordinal, Opus kbps, …).                                            |
| `chunk_size`    | [`ModemPreset::recommended_chunk_size()`](../../crates/voicetastic-core/src/voice/types.rs#L106). |
| `parity_count`  | 10 % short / 20 % medium / 33 % long / 50 % broadcast — round up.                         |
| `encryption`    | `Some(derive_key(psk, message_id, my_node_num))` if PSK present.                          |
| `last_in_stream`| `true` only on the very last message of a recording session.                              |

Adjust `chunk_size` down by `GCM_NONCE_LEN + GCM_TAG_LEN = 28` bytes when
encryption is enabled (max 191 instead of 219).

---

## Step 2 — Build the message

```rust
use voicetastic_core::voice::{
    BuildConfig, VoiceCodec, build_message, random_message_id,
    ModemPreset, derive_key,
};

let cfg = BuildConfig {
    message_id: random_message_id(),
    stream_seq,
    codec: VoiceCodec::AmrNb,
    codec_param: 5, // MR795
    chunk_size: ModemPreset::MediumFast.recommended_chunk_size(),
    parity_count: 2,
    last_in_stream: true,
    encryption: psk.map(|p| derive_key(p, message_id, my_node_num)),
};

let encoded = build_message(&audio, &cfg)?;
```

`encoded.frames` is a `Vec<Vec<u8>>` in send order: all DATA frames, then
all PARITY frames.

---

## Step 3 — Transmit with pacing

```rust
let pacing = ModemPreset::MediumFast.pacing(); // 200 ms
svc.send_voice(&encoded, channel, dest, pacing).await?;
```

[`MeshService::send_voice`](../../crates/voicetastic-core/src/service/outbound.rs)
walks the frames, sleeping `pacing` between sends. If you implement your
own transmitter:

- Send each frame on `PortNum::PRIVATE_APP` (256).
- Set `want_ack = dest.is_some()` (no ACKs on broadcasts; the firmware
  drops broadcast ACK requests anyway).
- Sleep `pacing` between frames. **Default 500 ms** when the preset is
  unknown — under-pacing causes GATT busy errors and LoRa duty-cycle
  starvation.

### Pacing table

| Modem preset                  | Pacing  |
|-------------------------------|---------|
| `SHORT_TURBO`, `SHORT_FAST`   | 100 ms  |
| `SHORT_SLOW`, `MEDIUM_FAST`   | 200 ms  |
| `MEDIUM_SLOW`, `LONG_FAST`    | 350 ms  |
| `LONG_MODERATE`, `LONG_SLOW`  | 500 ms  |
| `VERY_LONG_SLOW`              | 800 ms  |
| Unknown                       | 500 ms  |

Local transports (USB-serial, BLE-to-radio) don't have duty-cycle limits
themselves but the radio's outbound queue benefits from the same pacing.

---

## Step 4 — Handle NACK responses

> **Status:** the receive→sender NACK forwarding loop is wired in the CLI
> listener; the sender-side state machine that consumes NACKs and
> retransmits is **not yet implemented**. Treat this section as a forward
> plan.

When you receive a `PRIVATE_APP` payload addressed to you with
`packet_type = NACK` matching one of your in-flight `message_id`s:

1. Parse via
   [`parse_nack_body`](../../crates/voicetastic-core/src/voice/nack.rs).
2. If `give_up`: drop any remaining queued chunks for that `message_id`.
3. Otherwise, re-build the missing DATA frames listed in `missing` and
   retransmit. You MAY add additional parity beyond the original
   `parity_count` (the bitmap is sized solely from `total_data` so this
   does not change the NACK shape).
4. Pace retransmits per the same table.

---

## Optional optimizations

### Silence skipping

If a DATA chunk's payload is entirely codec NO_DATA frames (e.g. AMR-NB
all `0x7C`), you MAY skip transmission. The receiver sees a missing chunk
and either reconstructs it via FEC or zero-fills it on timeout — both
yield silence in the codec.

Receivers cannot distinguish silence-skipped chunks from lost chunks, so
this is purely a sender-side bandwidth save.

### Cancellable sends

Wrap your send loop in a `CancellationToken` so the user can abort. On
cancel, set `last_in_stream = 1` on the next sent frame to let the
receiver expire stream state early.

---

## Hard limits

| Constraint            | Plaintext | Encrypted |
|-----------------------|-----------|-----------|
| `chunk_size`          | 16..=219  | 16..=191  |
| `total_data`          | 1..=255   | 1..=255   |
| `parity_count`        | 0..=128   | 0..=128   |
| Max audio per message | 55 845 B  | 48 705 B  |

Send `audio.len() > 255 * chunk_size` and `build_message` returns
[`VoiceError::AudioTooLarge`](../../crates/voicetastic-core/src/voice/error.rs).

→ Continue to [Receiver Guide](Receiver-Guide.md).
