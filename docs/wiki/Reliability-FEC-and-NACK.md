# Reliability — FEC and NACK

[← Home](Home.md)

The voice protocol uses a two-stage reliability scheme:

1. **Reed-Solomon FEC** absorbs sub-`parity_count` losses with no
   round-trip.
2. **Selective NACK** with bitmap recovers larger losses in a single
   round-trip.

The combination keeps best-case latency low while bounding worst-case
airtime.

---

## Reed-Solomon FEC

Implementation: [`reed-solomon-erasure`](https://docs.rs/reed-solomon-erasure)
crate over **GF(2⁸)**, with `(total_data, parity_count)` shards.

### Sender

1. Split audio into `total_data` chunks of `chunk_size` bytes (zero-pad
   the last chunk).
2. RS-encode `parity_count` parity shards.
3. Send all `total_data + parity_count` shards (padding stripped on the
   final DATA frame; receivers re-pad for FEC math).

### Receiver

A receiver MUST be able to reconstruct the message if it has any
**`total_data`** shards out of the `total_data + parity_count` total —
any combination of DATA and PARITY shards counts toward the threshold.

### Choosing `parity_count`

`parity_count` is sender policy, expressed as a percentage of `total_data`:

| Mesh profile                          | `parity_count` |
|---------------------------------------|----------------|
| Short / quiet                         | 10 %           |
| Medium / mixed                        | 20 %           |
| Long / lossy                          | 33 %           |
| Broadcast (no NACK feedback channel)  | 50 %           |

`parity_count = 0` is allowed — it disables FEC entirely. NACK still
works.

### Why GF(2⁸)?

Byte-aligned, 256-shard ceiling fits perfectly in `u8` indices. No
bit-shuffle pre/post processing. Throughput on commodity hardware is well
above what LoRa airtime can deliver.

---

## Selective NACK

When loss exceeds `parity_count`, the receiver issues a NACK after a
**quiet period** of `NACK_WINDOW_MS` (default 1500 ms) since the last
chunk arrived for that message.

### Bitmap

A bitmap of length `⌈total_data / 8⌉` lists missing DATA chunks (one bit
per data shard). The sender retransmits only those chunks. On a single
NACK round, all missing chunks are listed in **one** bitmap — there's no
chunk-by-chunk retry.

### Round budget

`NACK_MAX_ROUNDS = 400` per message of **consecutive rounds without
progress** — the counter is reset every time a new shard lands, so a
sender that's still actively servicing every NACK round keeps the
assembly slot alive indefinitely (capped only by `message_timeout`,
default 600 s). After 400 NACK rounds in a row with zero new chunks the
receiver gives up and either emits a partial message
(`partial_play_on_timeout = true`, the default) or discards the work.
At a `nack_window` of 1500 ms that's a ~600 s ceiling on a truly silent
sender.

> Earlier revisions used a *cumulative* counter that never reset. That
> turned out to be indistinguishable from genuine silence on healthy
> slow-trickle messages — a 67-chunk Long-Slow broadcast could rack up
> 32 productive rounds long before delivery and surface a phantom
> `partial: 47/51 chunks` line. The consecutive semantic preserves the
> protection against a chatty bad sender (`message_timeout` is still
> the absolute upper bound) without the false positive.

### Empty NACK = positive ACK

A NACK whose bitmap is all zeros means "all chunks received, stop sending
parity". Parsers MUST accept this; the reference implementation doesn't
currently emit it (natural completion + the recently-finalized blacklist
already handle late parity frames).

### `give_up` flag

`flags & 0x01` = the receiver has timed out. Senders SHOULD discard any
remaining queued chunks for this message — keep transmitting and you
just waste airtime.

---

## End-to-end flow

```
Sender                                                 Receiver
──────                                                 ────────
build_message(audio)                                       │
  ├─ split into N data chunks                              │
  └─ RS-encode P parity chunks                             │
       │                                                   │
       ├─ DATA[0]    ─────────────────────────────────►    │
       ├─ DATA[1]    ─────X (lost)                         │
       ├─ DATA[2]    ─────────────────────────────────►    │ pending
       ├─ PARITY[0]  ─────────────────────────────────►    │ FEC reconstructs DATA[1]
       │                                                   │ ✓ Complete
       │                                                   │
       │   --- or, on heavier loss ---                     │
       │                                                   │
       ├─ DATA[0]    ─────X                                │
       ├─ DATA[1]    ─────X                                │
       ├─ DATA[2]    ────────────────────────────────►     │
       ├─ PARITY[0]  ─────X                                │
       │                                                   │ quiet 1500 ms
       │   ◄──────────────────────────  NACK [bitmap=0xC0]  │ (chunks 0 & 1 missing)
       ├─ DATA[0]    ────────────────────────────────►     │
       ├─ DATA[1]    ────────────────────────────────►     │ ✓ Complete
```

---

## Sender-side state machine

The sender side of NACK handling lives in
[`OutgoingVoiceRegistry`](../../crates/voicetastic-core/src/voice/outgoing.rs)
and is driven by [`VoiceSender`](../../crates/voicetastic-core/src/voice/sender.rs).
[`VoiceSender::send`] registers an entry at burst start and the
background NACK-listener task feeds inbound NACKs into
`take_retransmit`, which enforces three caps:

1. **Pending-chunk dedup.** Every DATA index is marked *pending* at
   `register()` time. The burst loop calls `mark_chunk_sent(i)` per
   frame as it leaves the worker. A NACK arriving while the initial
   burst is still draining is filtered against `pending_chunks`, so
   chunks already queued up are not re-enqueued.
2. **Per-message cooldown.** After each retransmit batch, the entry is
   parked for `pacing × frames.len()`, clamped to `[1 s, 30 s]`. The
    30 s ceiling sits comfortably below the receiver's ~600 s NACK
    budget so the sender always responds before the receiver gives up.
   NACK rounds during cooldown are dropped; the receiver re-NACKs
   after the next quiet window.
3. **Per-message retransmit budget.** `MAX_RETRANSMITS_PER_MESSAGE = 2_400`
    comfortably exceeds the widened receiver round cap (400 × max 30 s
    cooldown). Beyond that, the sender drops further NACKs.

After the initial burst, the sender lingers for `SendRequest::linger`
(default 600 s, see [`DEFAULT_LINGER`](../../crates/voicetastic-core/src/voice/sender.rs))
before emitting `SendStatus::Complete` and releasing registry state.
A stale NACK arriving after that point finds nothing to retransmit
against. The previous value of 60 s was too short for slow modem presets
(e.g. LongFast at 900 ms pacing: a 155-frame burst alone takes ~140 s,
leaving insufficient linger time for NACK-driven retransmit rounds).

The absolute outer envelope is `OutgoingVoiceRegistry::set_retain_ttl`
(default 1200 s, covering `max_burst_duration + linger` on all
modem presets the receiver can still hear, so a NACK never finds its
registry entry expired while the sender is still alive).

---

## NACK trust model

This protocol does not authenticate NACK frames; like DATA / PARITY, any
peer with the channel PSK (i.e. anyone Meshtastic lets join the channel)
can fabricate a `give_up` NACK and abort an in-flight transmission. This
matches Meshtastic's threat model for text traffic and is documented as
a non-goal in the spec. Mitigations available to senders:

- Treat `give_up` as advisory; if airtime budget allows, retry under a
  fresh `message_id` after a backoff.
- A future revision MAY reintroduce a keyed MAC field if a concrete
  threat model warrants it. See git history at the v2 line for the
  previous design.

---

## Tunable knobs

| Setting                            | Default | Effect of increasing                       |
|------------------------------------|---------|--------------------------------------------|
| `parity_count` (sender)            | 10–50 % | Better loss tolerance, more airtime        |
| `NACK_WINDOW_MS`                   | 1500    | Fewer spurious NACKs on jittery links      |
| `NACK_MAX_ROUNDS` (consecutive)    | 400     | Higher completion rate, longer worst-case  |
| `MAX_RETRANSMITS_PER_MESSAGE`      | 2_400   | Sender-side counterpart to NACK budget     |
| `AssemblerConfig::message_timeout` | 1200 s  | Larger messages allowed, more state held   |
| `OutgoingVoiceRegistry::retain_ttl`| 600 s   | Sender remembers frames longer             |
| `SendRequest::linger`              | 60 s    | Sender stays subscribed to NACKs longer    |
| `partial_play_on_timeout`          | `true`  | Always emits something on timeout          |

→ Continue to [Encryption](Encryption.md).
