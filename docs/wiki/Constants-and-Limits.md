# Constants and Limits

[← Home](Home.md)

Every numeric ceiling in the protocol, with rationale. The
[`consts`](../../crates/voicetastic-core/src/voice/consts.rs) module is
the source of truth for these values.

---

## Wire format

| Constant                | Value | Why                                                          |
|-------------------------|------:|--------------------------------------------------------------|
| `PROTOCOL_VERSION`      | `0x01` | Drop frames with any other first byte.                      |
| `HEADER_SIZE`           | 12 B  | Fixed header: 1 + 1 + 4 + 1 + 1 + 1 + 1 + 1 + 1.            |
| `MAX_PACKET_SIZE`       | 231 B | Meshtastic LoRa MTU. All frames MUST fit.                   |
| `MAX_BODY_SIZE`         | 219 B | `MAX_PACKET_SIZE − HEADER_SIZE`.                            |
| `MIN_CHUNK_SIZE`        | 16 B  | Per-frame overhead floor; below this, FEC + pacing waste airtime. |

## Message shape

| Constant                  | Value      | Why                                                        |
|---------------------------|-----------:|------------------------------------------------------------|
| `MAX_CHUNKS_PER_MESSAGE`  | 255        | `total_data` is `u8`; index 0..=254.                       |
| `MAX_PARITY_PER_MESSAGE`  | 128        | `reed-solomon-erasure` GF(2⁸) coder limit.                 |
| `MAX_MESSAGE_BYTES`       | 55 845     | `MAX_CHUNKS_PER_MESSAGE × MAX_BODY_SIZE`.                  |

With encryption enabled, the effective body limit drops by
`GCM_NONCE_LEN + GCM_TAG_LEN = 28 B`, so:

```
chunk_size_max(encrypted) = 219 − 12 − 16 = 191 B
max_audio(encrypted)       = 255 × 191    = 48 705 B
```

## Encryption

| Constant         | Value | Why                                                                      |
|------------------|------:|--------------------------------------------------------------------------|
| `GCM_NONCE_LEN`  | 12 B  | 96-bit nonce per RFC 5288 / NIST SP 800-38D recommendation.              |
| `GCM_TAG_LEN`    | 16 B  | 128-bit auth tag per AES-GCM standard.                                   |

Key derivation: HKDF-SHA256, `salt = channel_psk`, `ikm = message_id_be ‖ from_node_num_be`,
`info = "voicetastic/v2"`. See [Encryption](Encryption.md).

## Receiver resource bounds

| Constant                       | Value          | Why                                                          |
|--------------------------------|---------------:|--------------------------------------------------------------|
| `MAX_IN_PROGRESS_GLOBAL`       | 64             | Bounds total reassembler memory.                             |
| `MAX_IN_PROGRESS_PER_SENDER`   | 4              | Stops one chatty peer from starving everyone else.           |
| `BLACKLIST_TTL`                | 60 s           | How long a finalized message blocks late frames for itself.  |
| `BLACKLIST_MAX`                | 100            | FIFO eviction once exceeded.                                 |
| `NACK_MAX_ROUNDS`              | 3              | Per-message NACK budget before the receiver gives up.        |
| `NACK_WINDOW_MS`               | 1500           | Quiet period after the last seen chunk before NACK'ing.      |
| `MAX_VALIDATION_STRIKES` (impl)| 3              | Eviction trigger for chatty bad senders (post-template).     |

## Sender pacing

Adaptive per modem preset (`Config.LoRaConfig.modem_preset`):

| Modem preset                  | Pacing  |
|-------------------------------|--------:|
| `SHORT_TURBO`, `SHORT_FAST`   |  100 ms |
| `SHORT_SLOW`, `MEDIUM_FAST`   |  200 ms |
| `MEDIUM_SLOW`, `LONG_FAST`    |  350 ms |
| `LONG_MODERATE`, `LONG_SLOW`  |  500 ms |
| `VERY_LONG_SLOW`              |  800 ms |
| Unknown                       |  500 ms |

## Recommended `chunk_size` per preset

| Modem preset class                    | `chunk_size` |
|---------------------------------------|-------------:|
| Short-range (high SNR margin)         |          219 |
| Medium-range                          |          160 |
| Long-range                            |           96 |
| Very long-range (worst loss profile)  |           48 |

## Recommended `parity_count`

Expressed as a percentage of `total_data`:

| Mesh profile                          | `parity_count` |
|---------------------------------------|---------------:|
| Short / quiet                         |           10 % |
| Medium / mixed                        |           20 % |
| Long / lossy                          |           33 % |
| Broadcast (no NACK feedback channel)  |           50 % |

## Capacity reference

For quick sanity-checking message budgets:

| `chunk_size` | Codec / bitrate         | `max_audio`   | Approx. duration |
|-------------:|-------------------------|--------------:|-----------------:|
| 219          | OPUS @ 16 kbps          | 55 845 B      | ~28 s            |
| 191          | OPUS @ 16 kbps (encrypted) | 48 705 B   | ~24 s            |
| 160          | AMR-NB @ MR795 (7.95 kbps) | 40 800 B   | ~41 s            |
| 96           | AMR-NB @ MR795          | 24 480 B      | ~25 s            |
| 48           | AMR-NB @ MR795          | 12 240 B      | ~12 s            |

Durations are rough — they assume packed codec frames with no padding.

→ Continue to [Error Catalogue](Error-Catalogue.md).
