# Sender Guide

[← Home](Home.md)

How to ship a voice message — using the high-level
[`VoiceSender`](../../crates/voicetastic-core/src/voice/sender.rs) pipeline,
which now owns the entire wire-protocol state machine.

---

## TL;DR — the new way

Frontends no longer hand-roll the build → register → burst → NACK →
retransmit → linger loop. They submit a `SendRequest` to a single
shared `VoiceSender` and consume `SendStatus` events:

```rust
use voicetastic_core::voice::{SendRequest, SendStatus, VoiceCodec, VoiceSender};

// One per `MeshService` for the lifetime of the app.
let sender = VoiceSender::new(svc.clone());

let handle = sender.send(SendRequest {
    audio,                          // raw codec bytes, no container header
    codec: VoiceCodec::AmrNb,
    codec_param: 5,                 // AMR mode 5 = MR795
    channel: 0,
    to: Some(node_num),             // None ⇒ broadcast
    parity_count: 4,
    ..Default::default()
})?;

let mut rx = handle.subscribe();
while let Ok(status) = rx.recv().await {
    match status {
        SendStatus::Sending { sent, total, .. } => println!("{sent}/{total}"),
        s if s.is_terminal() => break,
        _ => {}
    }
}
```

The sender:

1. Builds wire frames via
   [`build_message`](../../crates/voicetastic-core/src/voice/builder.rs).
2. Registers them with an internal
   [`OutgoingVoiceRegistry`](../../crates/voicetastic-core/src/voice/outgoing.rs)
   so NACK rounds can be serviced.
3. Spawns a paced-burst task that ships every frame through the
   QueueStatus-gated voice TX worker.
4. A single per-service NACK-listener task watches the inbound data
   broadcast for NACKs targeting any in-flight `message_id` and
   dispatches retransmits.
5. Lingers for `linger` (default 60 s) after burst-complete so late
   NACK rounds can still be honoured, then emits `Complete` and
   releases registry state.

One `VoiceSender` instance handles every concurrent send for a service.

---

## SendRequest fields

| Field            | Type                  | Meaning                                                                  |
|------------------|-----------------------|--------------------------------------------------------------------------|
| `audio`          | `Vec<u8>`             | Raw codec frame bytes. Strip container headers (e.g. AMR `#!AMR\n`).     |
| `codec`          | `VoiceCodec`          | `AmrNb`, `Opus`, …                                                       |
| `codec_param`    | `u8`                  | Codec-specific (AMR mode ordinal, Opus kbps, …).                         |
| `channel`        | `u32`                 | Meshtastic channel index. `0` = primary.                                 |
| `to`             | `Option<u32>`         | Unicast node number; `None` = channel broadcast.                         |
| `parity_count`   | `u8`                  | RS parity shards. `0` disables FEC (NACK still works).                   |
| `chunk_size`     | `Option<usize>`       | Per-frame body size override. `None` ⇒ `MAX_BODY_SIZE` (219 B).          |
| `encryption`     | `Option<EnvelopeKey>` | AES-GCM envelope key. `None` ⇒ plaintext bodies.                         |
| `linger`         | `Option<Duration>`    | How long to keep the entry alive for late NACKs. `None` ⇒ 60 s.          |
| `stream_seq`     | `u8`                  | Per-(from, channel) monotonic counter. `0` is fine for one-shots.        |
| `last_in_stream` | `bool`                | Marks the final frame of a recording session.                            |
| `pacing`         | `Option<Duration>`    | Inter-frame TX pacing override. `None` ⇒ live modem preset.              |

### Picking `parity_count`

Sender policy, as a percentage of `total_data` (chunks):

| Mesh profile                          | `parity_count` |
|---------------------------------------|----------------|
| Short / quiet                         | 10 %           |
| Medium / mixed                        | 20 %           |
| Long / lossy                          | 33 %           |
| Broadcast (no NACK feedback channel)  | 50 %           |

Real LoRa broadcasts can sit at 30–45 % per-chunk loss; a fixed `8`
parity shards on a 60-chunk message is rarely enough.

### Picking `chunk_size`

`MAX_BODY_SIZE` (219 B) is best on short-range presets. For long-range
presets, a smaller `chunk_size` makes each loss cost less and gives FEC
finer granularity. See
[`ModemPreset::recommended_chunk_size`](../../crates/voicetastic-core/src/voice/types.rs).

Encryption shrinks the effective body by `GCM_NONCE_LEN + GCM_TAG_LEN = 28 B`
(max 191 B instead of 219 B).

---

## SendStatus events

```text
Building       { message_id, total_data, parity_count }   // wire frames built
Sending        { message_id, sent, total }                // one more frame on the worker
BurstComplete  { message_id, packet_ids }                 // initial burst all enqueued
Retransmitting { message_id, chunks }                     // NACK round serviced
Complete       { message_id }                             // linger expired cleanly
GaveUp         { message_id }                             // receiver sent give_up
Failed         { message_id, message }                    // unrecoverable error
```

The stream **always** terminates with exactly one of `Complete`,
`GaveUp`, or `Failed`. Use `SendStatus::is_terminal()` to break.

`Sending::sent` includes both DATA and PARITY frames; if you render a
chunk-count progress bar, cap it at `total_data`.

---

## Runtime context

`VoiceSender::new(svc)` must be called from inside an entered tokio
runtime. For frontends that build the sender on a non-runtime thread
(egui UI, JNI callbacks, etc.) use:

```rust
let sender = VoiceSender::new_on(svc.clone(), rt.handle().clone());
```

This captures the runtime handle at construction so all internal spawns
work regardless of caller thread.

---

## Tuning the retransmit registry

`VoiceSender::set_retain_ttl(Duration)` controls how long the internal
`OutgoingVoiceRegistry` keeps frames after the linger window. Wire
this to the same setting that drives the receiver's
`AssemblerConfig::message_timeout` (default 600 s) so a NACK never
arrives for a frame the sender has already forgotten.

The [Reliability page](Reliability-FEC-and-NACK.md#sender-side-state-machine)
explains the per-message cooldown and pending-chunk dedup that protect
against overlapping NACK rounds saturating the firmware queue.

---

## Pacing table

Read off `Config.LoRaConfig.modem_preset`. `VoiceSender` does this for
you when `SendRequest::pacing` is `None`.

| Modem preset                  | Pacing  |
|-------------------------------|---------|
| `SHORT_TURBO`, `SHORT_FAST`   | 100 ms  |
| `SHORT_SLOW`, `MEDIUM_FAST`   | 200 ms  |
| `MEDIUM_SLOW`, `LONG_FAST`    | 350 ms  |
| `LONG_MODERATE`, `LONG_SLOW`  | 500 ms  |
| `VERY_LONG_SLOW`              | 800 ms  |
| Unknown                       | 500 ms  |

Local transports (USB-serial, BLE-to-radio) don't have duty-cycle limits
themselves, but the radio's outbound queue benefits from the same
pacing.

---

## Hard limits

| Constraint            | Plaintext | Encrypted |
|-----------------------|-----------|-----------|
| `chunk_size`          | 16..=219  | 16..=191  |
| `total_data`          | 1..=255   | 1..=255   |
| `parity_count`        | 0..=128   | 0..=128   |
| Max audio per message | 55 845 B  | 48 705 B  |

`audio.len() > 255 × chunk_size` ⇒
[`VoiceError::AudioTooLarge`](../../crates/voicetastic-core/src/voice/error.rs)
returned synchronously from `send()`.

---

## Direct-frame APIs (rarely needed)

If you need raw control — e.g. testing, custom retransmit policy — you
can still use:

- [`build_message`](../../crates/voicetastic-core/src/voice/builder.rs)
  to produce wire frames without registering them.
- [`MeshService::enqueue_voice_frame_with_id`](../../crates/voicetastic-core/src/service/voice_tx.rs)
  to push a single frame through the paced TX worker.
- [`OutgoingVoiceRegistry`](../../crates/voicetastic-core/src/voice/outgoing.rs)
  directly if you want to manage retransmit state yourself.

For 99 % of frontends, `VoiceSender::send` is the right entry point.

### Foreign bindings (Android / Kotlin)

The UniFFI surface mirrors the Rust API:

- `MeshService.voiceSender()` returns the lazy shared `VoiceSender`.
- `VoiceSender.send(SendRequestUdl, listener)` returns the assigned
  `message_id`; `listener.onStatus(SendStatus)` fires lifecycle events
  on a Rust worker thread.
- See [`voicetastic.udl`](../../crates/voicetastic-android-bridge/src/voicetastic.udl)
  for the full dictionary / enum / interface definitions.

→ Continue to [Receiver Guide](Receiver-Guide.md).
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
