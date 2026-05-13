# Receiver Guide

[← Home](Home.md)

How to build a compatible Voicetastic reassembler — protocol-level
checklist plus reference-implementation pointers.

---

## Inbound dispatch

You'll be receiving Meshtastic `Data` packets. Filter:

```rust
if data.portnum != PRIVATE_APP as i32 { return; }   // 256
if detect_version(&data.payload) != Some(0x02) { return; }
```

The first-byte version check lets future protocol revisions co-exist on
the same port without breaking older clients.

---

## Setup

```rust
use voicetastic_core::voice::{AssemblerConfig, VoiceAssembler};
use std::time::Duration;

let asm = VoiceAssembler::new(AssemblerConfig {
    message_timeout: Duration::from_secs(30),
    partial_play_on_timeout: true,
    channel_psk: Some(channel_psk.to_vec()), // None ⇒ encrypted frames are dropped
});
```

| Field                       | Default       | Meaning                                                       |
|-----------------------------|---------------|---------------------------------------------------------------|
| `message_timeout`           | 30 s          | Hard timeout per in-progress message.                         |
| `partial_play_on_timeout`   | `true`        | Emit incomplete messages on timeout (vs. discard).            |
| `channel_psk`               | `None`        | If `Some`, used to derive AES-GCM envelope keys.              |

---

## Per-frame ingest

```rust
match asm.accept(&from_id, to, channel, &payload) {
    AssemblyEvent::Pending      => {}                         // accumulating
    AssemblyEvent::Duplicate    => {}                         // already had this chunk
    AssemblyEvent::Complete(m)  => handle_voice(*m).await?,   // done!
    AssemblyEvent::Nack(info)   => route_to_sender(info),     // we're hearing the other end's NACK
    AssemblyEvent::Rejected(e)  => warn!(?e, "bad frame"),
}
```

`from_id` MUST be the canonical lowercase `!hex8` form
(`voicetastic_core::ids::node_num_to_id`); the assembler parses it
strictly when the encryption envelope is involved.

---

## Periodic tick

You MUST drive `tick()` periodically to handle timeouts and emit NACKs:

```rust
let mut tick = tokio::time::interval(Duration::from_millis(250));
loop {
    tokio::select! {
        _ = tick.tick() => {
            let out = asm.tick();
            for completed in out.finalized {
                handle_voice(completed).await?;
            }
            for nack in out.nacks {
                let to_node = node_id_to_num(&nack.from)?;
                svc.send_data(
                    PRIVATE_APP as i32,
                    nack.frame,
                    nack.channel,
                    Some(to_node),
                    /* want_ack */ false,
                ).await?;
            }
        }
        // ... your inbound recv loop here
    }
}
```

Recommended cadence: **100–250 ms**. Below that you spin needlessly;
above that you delay NACK emission past the spec's
`NACK_WINDOW_MS = 1500`.

---

## Resource bounds

The assembler enforces all of these for you:

| Bound                              | Default     | What happens at the limit                                                                |
|------------------------------------|-------------|------------------------------------------------------------------------------------------|
| `MAX_IN_PROGRESS_GLOBAL`           | 64          | Oldest `started_at` entry evicted + blacklisted.                                         |
| `MAX_IN_PROGRESS_PER_SENDER`       | 4           | New messages from that sender rejected with `PerSenderCap`.                              |
| `MAX_MESSAGE_BYTES`                | 55 845      | Structurally bounded (`u8 × MAX_BODY_SIZE`).                                             |
| `BLACKLIST_TTL`                    | 60 s        | After a message completes/times out, late frames for it are silently dropped.            |
| `BLACKLIST_MAX`                    | 100         | Oldest entries evicted FIFO.                                                             |
| `NACK_MAX_ROUNDS`                  | 32          | After 32 *consecutive* NACK rounds with no new chunks, finalize-or-discard. Resets on every accepted shard.                              |
| `NACK_WINDOW_MS`                   | 1500        | Quiet period after the last seen chunk before emitting a NACK.                           |
| `MAX_VALIDATION_STRIKES` (impl)    | 3           | After 3 post-template mismatches, the entry is evicted + blacklisted.                    |

---

## Rejection rules (silent drops)

The assembler returns `AssemblyEvent::Rejected(VoiceError::*)` for:

- `version != 1`
- `packet_type == 3` (reserved)
- `total_data == 0`
- `chunk_index >= total_data` (DATA) or `>= parity_count` (PARITY)
- `parity_count > 128`
- Unknown `codec`
- Codec / `total_data` / `stream_seq` mismatch versus the established
  template
- DATA / PARITY body length mismatch versus the established `chunk_size`
- AES-GCM tag failure
- `encrypted = 1` but no PSK configured, or `from` is not strict `!hex8`
- `(from, message_id)` is on the recently-completed blacklist
- New `message_id` while the sender is at `MAX_IN_PROGRESS_PER_SENDER`

See [Error Catalogue](Error-Catalogue.md) for one-line descriptions of
each variant.

---

## Handling completed messages

```rust
async fn handle_voice(msg: VoiceMessage) {
    if !msg.is_complete {
        // Partial — playback may stutter. recovered_via_fec tells you
        // how many chunks the FEC layer saved.
    }
    if msg.received_data == 0 {
        // Pure-zero playback. Skip unless you really want a silent file.
        return;
    }
    // The audio is raw codec frames. For AMR-NB you'd write:
    //   #!AMR\n  ‖  msg.audio
    // before handing to a player.
}
```

The fields you'll usually inspect:

| Field                 | Meaning                                                |
|-----------------------|--------------------------------------------------------|
| `is_complete`         | All `total_data` chunks present (after FEC).           |
| `received_data`       | How many DATA chunks landed (incl. FEC reconstructions).|
| `recovered_via_fec`   | How many were reconstructed by Reed-Solomon.           |
| `encrypted`           | Whether any frame in this message was enveloped.       |
| `audio`               | Raw codec bytes, no container header.                  |

---

## Common pitfalls

- **Forgetting `tick()`.** Without it, NACKs are never emitted and
  partially-lost messages stall until `message_timeout`.
- **Passing the wrong `from`.** Use
  `voicetastic_core::ids::node_num_to_id(packet.from)` — the assembler's
  encryption path requires the strict `!hex8` form.
- **Accepting frames from non-`PRIVATE_APP` ports.** Other apps share the
  same Meshtastic radio; filter by `portnum == 256` *and* the version
  byte.
- **Wrapping the audio in a container header at the wrong layer.** The
  protocol carries raw codec frames. Wrap to `#!AMR\n` (or whatever) only
  when writing to disk for an external player.
- **Re-using `channel_psk` between channels.** The HKDF salt is the PSK,
  so a single PSK across two channels lets messages from one decrypt on
  the other if the message_id collides.

→ Continue to [Constants and Limits](Constants-and-Limits.md).
