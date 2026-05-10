# Frame Format

[← Home](Home.md)

Every voice frame is a Meshtastic `Data` payload sent on
`PortNum::PRIVATE_APP` (256). The payload is **at most 231 bytes** (LoRa
MTU) and consists of a fixed 12-byte header followed by an
optionally-encrypted body.

```
┌────────── 12 B header ───────────┬───── ≤ 219 B body ─────┐
│ version  │ type_flags │  fields  │       payload          │
└──────────┴────────────┴──────────┴────────────────────────┘
```

---

## Header (12 bytes)

| Off | Size | Field         | Notes                                                |
|-----|------|---------------|------------------------------------------------------|
|  0  |  1   | `version`     | `0x01`. Drop frames where this is anything else.    |
|  1  |  1   | `type_flags`  | bits 6–7 = `packet_type`, bit 5 = `encrypted`, bit 4 = `last_in_stream`, bits 0–3 reserved (must be 0). |
|  2  |  4   | `message_id`  | `u32` big-endian, non-zero, sender-chosen.           |
|  6  |  1   | `codec`       | See [codec table](#codec-table).                     |
|  7  |  1   | `codec_param` | Codec-specific (e.g. AMR-NB bitrate ordinal).        |
|  8  |  1   | `stream_seq`  | `u8` per-`(from, channel)` monotonic counter.        |
|  9  |  1   | `chunk_index` | `u8`. Range depends on `packet_type`.                |
| 10  |  1   | `total_data`  | `u8`. Number of original DATA chunks. `0` is reserved (rejected). |
| 11  |  1   | `parity_count`| `u8`. Number of FEC parity chunks (≤ 128).           |

### `type_flags` bit layout

```
   bit 7  bit 6     bit 5         bit 4           bits 3..0
   ┌──────────────┐ ┌─────────┐ ┌──────────────┐ ┌────────────┐
   │ packet_type  │ │encrypted│ │last_in_stream│ │  reserved  │
   └──────────────┘ └─────────┘ └──────────────┘ └────────────┘
     (2 bits)        (1 bit)     (1 bit)         (4 bits, =0)
```

| Field            | Mask   | Values                                  |
|------------------|--------|-----------------------------------------|
| `packet_type`    | `0xC0` | `0=DATA`, `1=PARITY`, `2=NACK`, `3` reserved |
| `encrypted`      | `0x20` | `1` ⇒ body is `nonce ‖ ciphertext ‖ tag`     |
| `last_in_stream` | `0x10` | `1` on the final frame of a recording session |
| reserved         | `0x0F` | MUST be 0; receivers MUST reject otherwise   |

---

## Packet types

### `DATA` (0)

The codec-frame payload for `chunk_index ∈ [0, total_data)`. All non-final
DATA chunks have body length exactly `chunk_size`. The final DATA chunk
(`chunk_index == total_data − 1`) MAY be shorter — the sender strips
zero-padding it added for FEC.

### `PARITY` (1)

A Reed-Solomon parity shard for `chunk_index ∈ [0, parity_count)`. Always
sized to `chunk_size`.

### `NACK` (2)

A selective-retransmit request. See [NACK frames](#nack-frames) below.

### Reserved (3)

`packet_type == 3` is reserved; receivers MUST drop these silently.

---

## Codec table

| `codec` | Name        | `codec_param` meaning                                       |
|---------|-------------|-------------------------------------------------------------|
| `0`     | `AMR_NB`    | AMR-NB bitrate ordinal (0..=7); see [AMR-NB rates](#amr-nb-bitrates) |
| `1`     | `OPUS`      | bitrate / 1000 (kbps); typical 6..=64 — advisory            |
| `2`     | `PCM_S16LE` | sample-rate index: 0 = 8 kHz, 1 = 16 kHz                    |
| `3..=255` | reserved  | receivers MUST drop                                         |

The protocol does **not** transcode. `codec_param` is metadata passed
through unchanged; receivers SHOULD interpret it per the column above but
the protocol does not range-check it.

### AMR-NB bitrates

| Ordinal | Mode  | kbps  | Frame bytes (incl. ToC) |
|--------:|-------|------:|------------------------:|
| 0       | MR475 |  4.75 |  13                     |
| 1       | MR515 |  5.15 |  14                     |
| 2       | MR59  |  5.90 |  16                     |
| 3       | MR67  |  6.70 |  18                     |
| 4       | MR74  |  7.40 |  20                     |
| 5       | MR795 |  7.95 |  21                     |
| 6       | MR102 | 10.20 |  27                     |
| 7       | MR122 | 12.20 |  32                     |

Default: **MR795** (ordinal 5). AMR-NB frame duration is 20 ms. The AMR
file header `#!AMR\n` is **not** carried on the wire — strip on send,
re-prepend on receive when writing files.

---

## Body layout

```
              encrypted=0                       encrypted=1
 ───────────────────────────  ─────────────────────────────────────
 plaintext payload bytes      12 B nonce ‖ ciphertext ‖ 16 B tag
```

For encrypted bodies, the nonce is randomly chosen per frame, prepended,
and authenticated against the **12-byte header as AAD**. See
[Encryption](Encryption.md) for the full envelope.

The unencrypted plaintext is:

- **DATA** — the codec frame bytes for this chunk.
- **PARITY** — the Reed-Solomon parity shard, sized to `chunk_size`.
- **NACK** — the bitmap structure below; never encrypted.

---

## NACK frames

A NACK carries the standard 12-byte header with these constraints:

- `packet_type = NACK`
- `encrypted = 0` (NACKs MUST NOT be enveloped)
- `chunk_index = 0`
- `message_id`, `codec`, `codec_param`, `stream_seq`, `total_data` echo
  the values of the message being NACK'd.
- `parity_count` MUST be echoed; consumers MAY ignore it (the bitmap is
  sized solely from `total_data`).

Body:

```
 Off    Size              Field          Encoding
 ──────────────────────────────────────────────────────────────────
   0      1               nack_version   UInt8 = 0x01
   1      1               flags          UInt8: bit 0 give_up,
                                                bits 1-7 reserved
   2  ⌈total_data/8⌉      bitmap         big-endian, bit `i` set ⇒
                                         chunk index `i` is missing
```

**Bit ordering.** Byte 2 bit 7 (MSB) = chunk 0; byte 2 bit 6 = chunk 1; …;
byte 2 bit 0 = chunk 7; byte 3 bit 7 = chunk 8; and so on.

**Empty bitmap** = positive ACK ("all received, stop sending parity").
The reference implementation does not currently emit this shape — it
relies on natural completion plus the per-message blacklist for late
frames — but parsers MUST accept it.

**`give_up`** = the receiver has timed out; the sender SHOULD discard any
remaining queued chunks for this message.

NACK transport: send via `PRIVATE_APP`, `want_ack=false`. NACKs are not
themselves retransmitted — the loss-recovery loop *is* the retransmission.

---

## Worked example

A 200-byte AMR-NB recording at MR795, sent on a `MEDIUM_FAST` channel
(chunk_size = 160), with `parity_count = 1`, plaintext:

```
total_data   = ceil(200 / 160) = 2
parity_count = 1
frames       = 2 DATA + 1 PARITY = 3 frames
```

| Frame | Header (hex)                       | Body              |
|-------|------------------------------------|-------------------|
| 0     | `01 00 dd cc bb aa 00 05 07 00 02 01` | 160 B AMR data    |
| 1     | `01 00 dd cc bb aa 00 05 07 01 02 01` | 40 B AMR data (last, trimmed) |
| 2     | `01 40 dd cc bb aa 00 05 07 00 02 01` | 160 B RS parity   |

Header field decode (frame 0):

- `01` — version
- `00` — `type_flags`: DATA, plaintext, not last_in_stream
- `dd cc bb aa` — message_id (BE) = `0xddccbbaa`
- `00` — codec = AMR_NB
- `05` — codec_param = MR795 ordinal
- `07` — stream_seq = 7
- `00` — chunk_index = 0
- `02` — total_data = 2
- `01` — parity_count = 1

→ Continue to [Reliability — FEC and NACK](Reliability-FEC-and-NACK.md).
