# Constants and Limits

[← Home](Home.md)

Every numeric ceiling in the protocol, with rationale. The
[`consts`](../../crates/voicetastic-core/src/voice/consts.rs) module is
the source of truth for these values.

---

## Wire format

| Constant                | Value | Why                                                          |
|-------------------------|------:|--------------------------------------------------------------|
| `PROTOCOL_VERSION`      | `0x03` | Drop frames with any other first byte.                      |
| `HEADER_SIZE`           | 16 B  | 12 B logical header + 4 B trailing integrity tag.           |
| `HEADER_MAC_LEN`        | 4 B   | Truncated unkeyed `SHA-256` over `header[0..12]`.           |
| `MAX_PACKET_SIZE`       | 231 B | Meshtastic LoRa MTU. All frames MUST fit.                   |
| `MAX_BODY_SIZE`         | 215 B | `MAX_PACKET_SIZE − HEADER_SIZE`.                            |
| `MIN_CHUNK_SIZE`        | 16 B  | Per-frame overhead floor; below this, FEC + pacing waste airtime. |

## Message shape

| Constant                  | Value      | Why                                                        |
|---------------------------|-----------:|------------------------------------------------------------|
| `MAX_CHUNKS_PER_MESSAGE`  | 255        | `total_data` is `u8`; index 0..=254.                       |
| `MAX_PARITY_PER_MESSAGE`  | 128        | `reed-solomon-erasure` GF(2⁸) coder limit.                 |
| `MAX_MESSAGE_BYTES`       | 54 825     | `MAX_CHUNKS_PER_MESSAGE × MAX_BODY_SIZE`.                  |

## Confidentiality

V3 has **no protocol-layer encryption**. Confidentiality is delegated
to Meshtastic's channel encryption (AES-256-CTR with the channel PSK).
See [Encryption](Encryption.md).

## Receiver resource bounds

| Constant                       | Value          | Why                                                          |
|--------------------------------|---------------:|--------------------------------------------------------------|
| `MAX_IN_PROGRESS_GLOBAL`       | 64             | Bounds total reassembler memory.                             |
| `MAX_IN_PROGRESS_PER_SENDER`   | 4              | Stops one chatty peer from starving everyone else.           |
| `BLACKLIST_TTL`                | 60 s           | How long a finalized message blocks late frames for itself.  |
| `BLACKLIST_MAX`                | 100            | FIFO eviction once exceeded.                                 |
| `NACK_MAX_ROUNDS`              | 400            | Per-message NACK budget (consecutive rounds without progress; resets on every accepted shard) before the receiver gives up. |
| `NACK_WINDOW_MS`               | 3000           | Quiet period after the last seen chunk before NACK'ing.      |
| `MAX_VALIDATION_STRIKES` (impl)| 3              | Eviction trigger for chatty bad senders (post-template).     |

## Sender resource bounds

| Constant                            | Value | Why                                                                 |
|-------------------------------------|------:|---------------------------------------------------------------------|
| `MAX_RETRANSMITS_PER_MESSAGE`       | 2_400 | Per-message retransmit budget; matches widened receiver `NACK_MAX_ROUNDS`.  |
| `DEFAULT_RETAIN_TTL`                | 1200 s | How long `OutgoingVoiceRegistry` keeps frames for late NACKs (burst + linger safety margin). |
| `DEFAULT_LINGER` (`SendRequest`)    | 600 s | How long `VoiceSender` stays subscribed to NACKs after burst end.   |
| Cooldown clamp                      | 1–30 s | Park window after each retransmit batch (`pacing × frames`).        |

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
| Long-range (LongFast)                 |          199 |
| Long-moderate (MediumSlow)           |           96 |
| Very long-range (worst loss profile)  |           48 |

## Recommended `parity_count`

Severe loss (>40 %) requires nearly 1:1 parity for Reed–Solomon to close
without relying on NACK retransmit rounds:

| Mesh profile                          | `parity_count`       |
|---------------------------------------|---------------------:|
| Short / quiet                         |               10 %   |
| Medium / mixed                        |               20 %   |
| Long / lossy                          |               33 %   |
| High-loss (>40 %) / broadcast         | 100 % (up to 128)    |

## Capacity reference

For quick sanity-checking message budgets:

| `chunk_size` | Codec / bitrate         | `max_audio`   | Approx. duration |
|-------------:|-------------------------|--------------:|-----------------:|
| 219          | OPUS @ 16 kbps          | 55 845 B      | ~28 s            |
| 160          | AMR-NB @ MR795 (7.95 kbps) | 40 800 B   | ~41 s            |
| 128          | AMR-NB @ MR795          | 32 640 B      | ~33 s            |
| 96           | AMR-NB @ MR795          | 24 480 B      | ~25 s            |
| 48           | AMR-NB @ MR795          | 12 240 B      | ~12 s            |

Durations are rough — they assume packed codec frames with no padding.

→ Continue to [Error Catalogue](Error-Catalogue.md).
