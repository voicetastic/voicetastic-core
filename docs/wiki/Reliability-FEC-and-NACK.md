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

`NACK_MAX_ROUNDS = 3` per message. After the third NACK without
completion, the receiver gives up and either emits a partial message
(`partial_play_on_timeout = true`, the default) or discards the work.

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
  ├─ RS-encode P parity chunks                             │
  └─ optional AES-GCM envelope per frame                   │
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

## NACK trust model

NACKs are deliberately **not** AES-GCM-enveloped. This keeps them small
(bitmap fits in 32 bytes for the maximum 255 chunks) and forwarder-debuggable.

The trade-off: a peer with the channel PSK can forge a `give_up` NACK and
abort an in-flight transmission. This is documented as a non-goal in the
spec. Mitigations available to senders:

- Treat `give_up` as advisory; if airtime budget allows, retry under a
  fresh `message_id` after a backoff.
- A future revision MAY add an HMAC field over the NACK body keyed by the
  envelope key. The current header has 4 reserved flag bits available.

---

## Tunable knobs

| Setting                       | Default | Effect of increasing                       |
|-------------------------------|---------|--------------------------------------------|
| `parity_count` (sender)       | 10–50 % | Better loss tolerance, more airtime        |
| `NACK_WINDOW_MS`              | 1500    | Fewer spurious NACKs on jittery links      |
| `NACK_MAX_ROUNDS`             | 3       | Higher completion rate, longer worst-case  |
| `message_timeout`             | 30 s    | Larger messages allowed, more state held   |
| `partial_play_on_timeout`     | `true`  | Always emits something on timeout          |

→ Continue to [Encryption](Encryption.md).
