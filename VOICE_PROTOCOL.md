# Voicetastic Voice Protocol

Voice messaging over the Meshtastic mesh.

This document is the **source of truth** for the wire format used by
`voicetastic-desktop` and any compatible peer. The reference implementation
lives in [`crates/voicetastic-core/src/voice/`](crates/voicetastic-core/src/voice/).

For an implementer-friendly companion (byte walkthroughs, diagrams,
sender/receiver recipes), see the [voice protocol wiki](docs/wiki/Home.md).

**Protocol version: 3 (wire byte `0x03`)**

V3 is wire-incompatible with V2: the body envelope encryption
(AES-256-GCM with a per-message HKDF-derived key) and the keyed-MAC
variant of the trailing header tag have been removed in favour of
Meshtastic's channel encryption. The four-byte trailing header tag is
now always unkeyed `SHA-256(header[0..12])[..4]` and serves as a
tamper-detection check (channel AES-CTR is bit-flip malleable, so this
remains useful even though the payload is encrypted on the wire).
A V3 parser MUST reject any frame whose `version` byte is `0x02`.

---

## 1. Goals

- Carry recorded voice (any narrowband codec) over Meshtastic LoRa packets.
- Survive packet loss without retransmission round-trips when possible (FEC).
- When loss exceeds FEC capacity, recover the missing frames with a single
  bitmap NACK exchange instead of full retransmission.
- Avoid head-of-line blocking: each voice message stands alone; partial
  playback is allowed if loss is unrecoverable.
- Be codec-agnostic so future codecs (Opus, Codec2, …) can ship without a
  protocol bump.
- Resist abuse: bound per-sender resource use, reject malformed frames early.
  Confidentiality is delegated to Meshtastic's channel encryption layer
  (AES-256-CTR with the channel PSK).

Receivers MUST drop any frame whose first byte is not [`PROTOCOL_VERSION`]
(`0x03`). The version byte exists so future revisions can coexist on the
same port without breaking older receivers.

---

## 2. Transport

Frames ride on standard Meshtastic data packets:

```protobuf
MeshPacket {
    from:     <sender node num>           // fixed32
    to:       <dest node num | 0xFFFFFFFF for broadcast>
    channel:  <channel index>
    decoded: Data {
        portnum: PRIVATE_APP                // = 256
        payload: <chunk bytes>              // header + body, ≤ 231 B
    }
    want_ack: <true for DMs of DATA/PARITY frames>
}
```

| Field      | Value                                                            |
|------------|------------------------------------------------------------------|
| Port       | `PRIVATE_APP` = **256**                                          |
| `to`       | Broadcast (`0xFFFFFFFF`) or specific node num                    |
| `channel`  | Currently selected Meshtastic channel index                      |
| `want_ack` | `true` for DM `DATA`/`PARITY`; `false` for broadcasts and `NACK` |

`MAX_PACKET_SIZE = 231` bytes (Meshtastic LoRa MTU). All frames MUST fit.

### 2.1 Adaptive pacing

Senders SHOULD wait between successive packets to avoid GATT busy errors and
LoRa duty-cycle starvation. The recommended delay depends on the radio's
**modem preset** (read from `Config.LoRaConfig.modem_preset`):

| Modem preset                  | Pacing  |
|-------------------------------|---------|
| `SHORT_TURBO`, `SHORT_FAST`   | 100 ms  |
| `SHORT_SLOW`, `MEDIUM_FAST`   | 200 ms  |
| `MEDIUM_SLOW`, `LONG_FAST`    | 350 ms  |
| `LONG_MODERATE`, `LONG_SLOW`  | 500 ms  |
| `VERY_LONG_SLOW`              | 800 ms  |

When the preset is unknown, senders MUST default to **500 ms**.

Local transports (USB-serial, BLE-to-radio) are not subject to LoRa
duty-cycle limits, but the radio's queue still benefits from pacing; the
recommended values above SHOULD be used regardless of the link to the
radio.

### 2.2 Firmware-queue backpressure

In addition to the time-based pacing above, senders SHOULD honour the
firmware's outbound queue depth, which Meshtastic devices advertise via
`FromRadio.QueueStatus { res, free, maxlen, mesh_packet_id }` after every
accept/drain. When `free` drops to a small low-water mark (the reference
implementation uses **2**), the sender MUST pause until the next
`QueueStatus` update before pushing another voice frame, with a safety
timeout (≈ 2 s) so a missed update can't stall transmission indefinitely.

Without this gate, a long voice burst can overflow the firmware's
outbound queue and trigger an out-of-memory reboot on the sender device.
NACK-driven retransmits MUST flow through the same paced + backpressured
path as the initial DATA/PARITY frames.

---

## 3. Frame format

Every frame is at most 231 bytes and starts with a 16-byte header
(12 logical bytes followed by a 4-byte MAC trailer):

```
 Byte  Size  Field            Encoding
 ───────────────────────────────────────────────────────────────────────
   0     1   version          UInt8 = 0x03
   1     1   type_flags       UInt8: bits 6-7 packet_type
                                     bit  5  reserved (must be 0; was V2 `encrypted`)
                                     bit  4  last_in_stream
                                     bit  3  reserved (must be 0; was V2 `mac_keyed`)
                                     bits 0-2 reserved (must be 0)
   2     4   message_id       UInt32, big-endian
   6     1   codec            UInt8 (see §3.2)
   7     1   codec_param      UInt8 (codec-specific, see §3.2)
   8     1   stream_seq       UInt8 (per-(from,channel) monotonic)
   9     1   chunk_index      UInt8
  10     1   total_data       UInt8 (1..=255, original data chunks; 0 is reserved and rejected — see §9.2)
  11     1   parity_count     UInt8 (0..=128, FEC parity chunks; 0 = no FEC, the default)
  12     4   mac_tag          SHA-256(header[0..12])[..4] — unkeyed integrity tag
 ───────────────────────────────────────────────────────────────────────
  16   ≤215  body             plaintext codec/parity/NACK bytes (see §3.3)
```

### 3.1 Packet types

`packet_type` is the top 2 bits of `type_flags`:

| Value | Name     | `body` content                                          |
|-------|----------|---------------------------------------------------------|
|   0   | `DATA`   | encoded audio for `chunk_index` (0..=total_data−1)      |
|   1   | `PARITY` | Reed-Solomon parity for `chunk_index` (0..=parity_count−1) |
|   2   | `NACK`   | bitmap of missing data chunk indices (see §3.4)         |
|   3   | reserved | receivers MUST drop                                     |

### 3.2 Codec field

| `codec` | Name        | `codec_param` meaning                                       |
|---------|-------------|--------------------------------------------------------------|
|   0     | `AMR_NB`    | AMR-NB bitrate ordinal (0..=7), see §3.2.1                  |
|   1     | `OPUS`      | bitrate / 1000 (kbps); typical range 6..=64, advisory only   |
|   2     | `PCM_S16LE` | sample rate index: 0=8 kHz, 1=16 kHz                         |
|   3     | `CODEC2`    | Codec2 mode ordinal, see §3.2.2                             |
| 4..255  | reserved    | receivers MUST drop unknown codecs                           |

`codec_param` is codec-specific metadata passed through unmodified by the
protocol; receivers SHOULD interpret it per the codec column above but the
protocol does not range-check it.

The codec/codec_param fields are advisory: the protocol does not transcode.
Receivers that do not support the advertised codec MUST drop the frame and
SHOULD surface a "codec unsupported" event to the application layer.
`codec` values in the `4..=255` range are reserved; receivers MUST drop
frames carrying them.

#### 3.2.1 AMR-NB bitrates

| Ordinal | Mode  | kbps  | Frame bytes (incl. ToC) |
|---------|-------|-------|--------------------------|
| 0       | MR475 | 4.75  | 13                       |
| 1       | MR515 | 5.15  | 14                       |
| 2       | MR59  | 5.90  | 16                       |
| 3       | MR67  | 6.70  | 18                       |
| 4       | MR74  | 7.40  | 20                       |
| 5       | MR795 | 7.95  | 21                       |
| 6       | MR102 | 10.2  | 27                       |
| 7       | MR122 | 12.2  | 32                       |

Default: `MR795`. Frame duration: 20 ms. The AMR file header `#!AMR\n` is
**not** carried on the wire; senders strip it before chunking and receivers
re-prepend it before writing files. The protocol body holds raw codec
frames only.

#### 3.2.2 Codec2 modes

| Ordinal | Mode  | bitrate | Frame bytes |
|---------|-------|---------|-------------|
| 0       | 3200  | 3.2 kbps | 8           |
| 1       | 2400  | 2.4 kbps | 6           |
| 2       | 1600  | 1.6 kbps | 8           |
| 3       | 1400  | 1.4 kbps | 7           |
| 4       | 1300  | 1.3 kbps | 7           |
| 5       | 1200  | 1.2 kbps | 6           |

Default: `1200`. Frame duration is mode-dependent (20–40 ms). Like AMR-NB,
the protocol body carries raw codec frames only; no container header.

### 3.3 Body layout

The body is always plaintext at this layer; confidentiality on the air
comes from Meshtastic's channel encryption wrapping the whole `Data`
payload (header + body) at the firmware level.

- `DATA` frames carry the codec frame bytes for this chunk.
- `PARITY` frames carry the Reed-Solomon parity for this chunk index,
  sized to `chunk_size` (see §4).
- `NACK` frames carry the bitmap described in §3.4.

### 3.4 NACK body

A NACK frame carries the standard 16-byte header, with these constraints:

- `packet_type = NACK` (binary `10` in `type_flags` bits 6–7)
- `chunk_index = 0`
- `message_id`, `codec`, `codec_param`, `stream_seq`, `total_data` MUST
  echo the values of the message being NACK'd. `parity_count` MUST be
  echoed for completeness; receivers of NACKs (i.e. the original sender)
  MAY ignore it since the bitmap is sized solely from `total_data`.

The body is:

```
 Byte   Size              Field         Encoding
 ─────────────────────────────────────────────────────────────────────
   0       1              nack_version  UInt8 = 0x01
   1       1              flags         UInt8: bit 0 give_up
                                                bits 1-7 reserved
   2  ⌈total_data/8⌉      bitmap        big-endian, bit `i` set ⇒ index `i` missing
```

Bit 0 (most-significant bit of byte 2) corresponds to chunk index 0; bit 7
to chunk index 7; byte 3 bit 0 to chunk index 8; and so on. A NACK with
all bitmap bits cleared is a **positive ACK** and the sender SHOULD stop
transmitting any remaining queued parity chunks for this message;
receivers in the reference implementation rely on natural completion (and
the per-message blacklist for late frames) and do not currently emit this
shape, but parsers MUST accept it. When `give_up` is set, the receiver
has timed out and the sender SHOULD discard any remaining queued chunks
for this message.

NACKs are not retransmitted (the loss-recovery loop is itself the
retransmission), and SHOULD be sent with `want_ack=false`.

**Broadcast suppression.** Receivers MUST NOT emit NACKs for messages
addressed to the broadcast address. With multiple listeners on the same
channel, every receiver NACK'ing the same missing chunks would flood
the sender, and the sender has no way to choose a retransmit target.
Broadcast voice relies on FEC (`parity_count`) and the
partial-on-timeout fallback. The reference receiver short-circuits the
NACK emission branch when `to == broadcast`.

**NACK backoff is host-tunable.** The reference receiver schedules NACK
rounds at `nack_window × backoff_base^min(round, 4)`. The base is
selected by the `voice.nack_mode` setting: typically `2` for
short/medium presets (doubling — faster retries), `3` for long-range
presets (tripling — gentler). `backoff_base = 0` disables NACK
emission entirely (used by `voice.nack_mode = off` and by the
broadcast short-circuit above).

---

## 4. Chunk size

Variable chunk size per message: each message picks a `chunk_size` ∈
`[16, 219]` based on the modem preset:

| Modem preset class                    | `chunk_size` |
|---------------------------------------|--------------|
| Short-range (high SNR margin)         | 219          |
| Medium-range                          | 160          |
| Long-range                            | 96           |
| Very long-range (worst loss profile)  | 48           |

`chunk_size` is **not** carried in the header — receivers infer it from
the first frame whose body length is unambiguous: any **PARITY** frame, or
any **non-final DATA** frame. A receiver MUST NOT freeze `chunk_size` from
a lone trimmed final DATA chunk that arrives first; it defers discovery
until one of the unambiguous frame types arrives. Once established,
later DATA frames whose body length differs (excluding the last data
chunk, which MAY be shorter) and PARITY frames whose body length differs
MUST be rejected.

The sender's recording duration limit derives from `chunk_size`:

```
max_audio_bytes(chunk_size) = chunk_size × 255  # 255 = max total_data
```

At `chunk_size = 219` and OPUS @ 16 kbps that's ~28 s of audio per message;
at `chunk_size = 48` and AMR-NB @ MR795 it's ~12 s.

---

## 5. Forward Error Correction

FEC uses **Reed-Solomon over GF(2⁸)** (`reed-solomon-erasure` crate) with
`(total_data, parity_count)` shards.

- The sender encodes `total_data` original chunks (zero-padded to
  `chunk_size`) and produces `parity_count` parity chunks. Padding is
  removed only on the final data chunk during reassembly.
- The receiver MUST be able to reconstruct the message if it has any
  `total_data` shards out of the `total_data + parity_count` total (any
  combination of DATA and PARITY shards counts toward the threshold).
  Loss up to `parity_count` shards is recoverable without a NACK.
- **Final-chunk caveat.** The trimmed length of the final DATA chunk is
  not carried on the wire (§3.3); receivers only learn it when they
  observe the real final DATA frame. If the final DATA chunk is the
  one missing, FEC alone cannot recover it without inventing trailing
  zero-padding. Receivers MUST NOT finalize a message via FEC when the
  final DATA shard's real length is unknown — they MUST either wait for
  the real frame (via NACK-driven retransmit) or fall back to a partial
  finalize on hard timeout. The reference implementation defers FEC
  recovery of the last shard in exactly this case.
- `parity_count` is sender policy; recommended values:

| Mesh profile      | `parity_count` (% of `total_data`) |
|-------------------|------------------------------------|
| Short / quiet     | 10 %                               |
| Medium / mixed    | 20 %                               |
| Long / lossy      | 33 %                               |
| Broadcast (no NACK feedback channel) | 50 % |

`parity_count = 0` is allowed and disables FEC entirely (NACK still works).

When loss exceeds `parity_count`, the receiver issues a NACK; the sender
retransmits the missing data chunks (only) and MAY add additional parity
chunks beyond the original `parity_count`. Retransmitted frames carry the
same `message_id`, `chunk_index`, and `total_data`. `parity_count` MAY
grow on retransmit; receivers MUST accept frames whose `parity_count` is
≥ the value first observed and SHOULD reject decreases. Retransmitted
DATA / PARITY frames are paced under the same rules as the original send
(see §2.1).

---

## 6. Identification keys

- **Message identity**: `(from_node_num, message_id)` where `message_id` is
  a non-zero `u32` chosen by the sender. The 32-bit space makes accidental
  collision with the receiver's recently-finalized blacklist negligible.
  `from` MUST be the lowercase 8-hex-digit form `!hex8`; ids should be
  normalized before lookup.
- **Stream identity**: `(from_node_num, channel, stream_seq)` where
  `stream_seq` is a `u8` monotonic counter per (sender, channel) pair,
  wrapping at 256. It is intended for receivers that order overlapping
  voice messages from the same sender on the same channel deterministically
  (e.g. interleaved recordings); the reference implementation currently
  treats it as informational and echoes it on NACK frames. The
  `last_in_stream` flag (bit 4 of `type_flags`) is set on the final frame
  of a recording session; receivers MAY use it to expire stream-history
  state for that sender. The reference implementation currently treats
  this bit as informational.

---

## 7. Confidentiality and integrity

Confidentiality is provided by Meshtastic's channel encryption layer:
the whole `Data` payload (this protocol's header + body) is encrypted
with AES-256-CTR using the channel PSK before being handed to the LoRa
PHY. Peers without the channel PSK cannot read the bytes.

This protocol layer does **not** add a per-message AEAD envelope. V2
carried one (AES-256-GCM with an HKDF-derived per-message key); it was
removed in V3 because Meshtastic's channel encryption already covers
the threat we care about (an outsider passively snooping the air) and
the GCM nonce + tag burned 28 octets of every chunk — meaningful on
LoRa where the body budget is ~199 bytes.

The 4-byte trailing `mac_tag` is `SHA-256(header[0..12])[..4]`,
**unkeyed**. It serves two purposes:

1. **Bit-flip detection.** AES-CTR is malleable: an attacker who can
   modify ciphertext bits on the air can flip the corresponding bits of
   the underlying plaintext header (`chunk_index`, `parity_count`,
   `message_id`, etc.). The SHA-256 truncated tag catches such tampering
   before any field is acted on.
2. **Accidental-corruption check.** On marginal links, occasional
   in-frame bit errors slip past LoRa FEC; the tag rejects them rather
   than silently corrupting the reassembly state.

The tag is **not** an authenticator: any peer who holds the channel PSK
(and can therefore both encrypt new frames and read existing ones) can
forge a valid mac_tag. Authentication of the audio stream against
malicious channel members is out of scope for this layer.

---

## 8. Send-side flow

```
                     ┌─────────────────┐
                     │ codec encoder   │ produces packed audio bytes
                     └────────┬────────┘
                              │
                              ▼
                     ┌─────────────────┐
                     │ split into      │ chunk_size from §4
                     │ total_data      │
                     │ chunks (last    │
                     │ may be padded)  │
                     └────────┬────────┘
                              │
                              ▼
                     ┌─────────────────┐
                     │ Reed-Solomon    │ produce parity_count parity shards
                     │ encode          │
                     └────────┬────────┘
                              │
                              ▼
                     ┌─────────────────┐
                     │ for each shard: │ build header, send via PRIVATE_APP
                     │   send_data     │ with adaptive pacing
                     │   want_ack=true │
                     │   for DM        │
                     └────────┬────────┘
                              │
                              ▼
                     ┌─────────────────┐
                     │ wait for NACK   │ up to nack_window_ms
                     │ window          │
                     └────────┬────────┘
                              │
                              ▼
                     ┌─────────────────┐
                     │ on NACK:        │ retransmit missing data chunks
                     │ rebuild missing │ (and optionally extra parity)
                     │ shards          │
                     └─────────────────┘
```

Senders MAY also drop **silence chunks**: a DATA chunk whose payload is
entirely codec NO_DATA frames does not need to be sent, since the receiver
synthesises silence for missing chunks anyway. Silence detection is
codec-specific (e.g. AMR-NB: all bytes equal to `0x7C`). Receivers MUST
NOT distinguish silence-skipped chunks from lost chunks: both appear as
"missing", are eligible for FEC reconstruction, and are zero-filled on
timeout per §9.

The full send is cancellable: a `CancellationToken` signal aborts
remaining transmissions and emits `last_in_stream = 1` on the next sent
frame.

---

## 9. Receive-side flow

```
chunk arrives
      │
      ▼
parse 16-byte header  ─────► reject if version != 3
                     ─────► reject if any reserved type_flags bit is set
                     ─────► reject if header MAC mismatches (§3)
      │
      ▼
check blacklist        ─────► reject if already finalized
      │
      ▼
lookup or create AssemblyState for (from, message_id)
      │
      ├─ NACK frame? ─────► route to send-side state for that message_id
      │
      ├─ new state → start internal timer (chunk_timeout_seconds)
      │              and per-sender rate-limit slot
      │
      ▼
store body at chunks[chunk_index] (DATA) or parity[chunk_index] (PARITY)
      │
      ├─ enough shards to RS-decode? ──► reconstruct missing data chunks
      │
      ├─ all data chunks present? ─────► finalize (complete) → emit VoiceMessage
      │
      ├─ partial timeout?           ─────► emit NACK with bitmap of missing
      │                                    chunks; reset timer
      │
      └─ hard timeout (after N NACK rounds)? ─► finalize (partial) or discard
                                                per partial_play_on_timeout
```

### 9.1 Resource bounds

- `MAX_IN_PROGRESS_GLOBAL = 64` total reassemblies. When the global cap
  is reached and a new `(from, message_id)` arrives, the receiver evicts
  the in-progress entry with the oldest `started_at` and blacklists its
  key for `BLACKLIST_TTL`.
- `MAX_IN_PROGRESS_PER_SENDER = 4` per `from_node_num` (prevents one chatty
  peer from starving everyone else).
- `MAX_MESSAGE_BYTES = 255 × 219 = 55_845` (worst-case sum of data shards
  before FEC overhead). Refused beyond this.
- `BLACKLIST_TTL = 600 s`, `BLACKLIST_MAX = 100`. This is the
  receiver's **completion-memory** window: once a `(from, message_id)`
  pair has finalized (complete or partial), late chunks for that pair
  are silently dropped for this long so the sender's firmware-queue
  drain (which can outrun the receiver's completion by tens of seconds
  on slow presets) doesn't resurrect a phantom partial reassembly.
  The window should be ≥ the assembler's `message_timeout`.
- `NACK_MAX_ROUNDS = 32` cumulative per message before the receiver
  gives up. This counter is *not* reset on progress: a sender that
  trickles one shard just before every quiet-window deadline must
  still finish within this many NACK rounds, otherwise the assembler
  finalizes partial. The previous value of `3` gave up after only
  ~4–5 s of quiet, which is far too aggressive on slow LoRa presets
  where inter-chunk gaps routinely exceed a second.
- `NACK_WINDOW_MS = 1500` after the last seen chunk before issuing a NACK.
  Global NACK emission is bounded by `MAX_IN_PROGRESS_GLOBAL` per tick;
  senders / transports SHOULD pace NACK transmission per §2.1.

### 9.2 Rejection rules

A receiver MUST reject (silently drop) a frame when **any** of the
following hold:

- `version != 0x03` (protocol version 3)
- any reserved bit of `type_flags` is set (`0x2F` mask — covers the
  former V2 `encrypted` (`0x20`) and `mac_keyed` (`0x08`) bits plus the
  three low reserved bits)
- `packet_type == 3` (reserved)
- `total_data == 0`
- `chunk_index >= total_data` for `DATA` frames
- `chunk_index >= parity_count` for `PARITY` frames
- `parity_count > 128`
- the frame's `codec` is unknown to the receiver (spec §3.2)
- the frame's `codec` differs from the codec established by the first
  frame of this `(from, message_id)`
- the frame's `total_data` differs from the value established by the first
  frame of this `(from, message_id)`
- the frame's `parity_count` is **less than** the value established by the
  first frame of this `(from, message_id)` (spec §5: it MAY grow on
  retransmit but MUST NOT shrink)
- DATA body length differs from the message's established `chunk_size` and
  this is not the last data chunk
- PARITY body length differs from the message's established `chunk_size`
- a NACK frame's `chunk_index` is non-zero (spec §3.4)
- the `(from, message_id)` is on the recently-completed blacklist
- the sender already has `MAX_IN_PROGRESS_PER_SENDER` in-flight messages
  and this is a new `message_id`

---

## 10. Data model (Rust)

```rust
pub struct VoiceMessage {
    pub message_id: u32,
    pub from: String,                       // "!aabbccdd"
    pub to: VoiceDestination,               // Node(u32) | Broadcast
    pub stream_seq: u8,
    pub codec: VoiceCodec,
    pub codec_param: u8,
    pub audio: Vec<u8>,                     // codec frame bytes, no container
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub is_complete: bool,
    pub total_data: u8,
    pub received_data: u8,
    pub recovered_via_fec: u8,              // chunks reconstructed by RS
    pub channel: u32,
}

pub enum VoiceCodec { AmrNb, Opus, PcmS16Le, Codec2, Unknown(u8) }
pub enum VoiceDestination { Node(u32), Broadcast }
```

The reassembled `audio` is the raw codec stream; the protocol does **not**
prepend any container header. Callers wrap the bytes in the appropriate
container themselves if needed (e.g. `#!AMR\n` for AMR-NB playback).

---

## 11. Limitations and design trade-offs

| Constraint                       | Value           | Reason                                  |
|----------------------------------|-----------------|-----------------------------------------|
| Max chunks per message           | 255 (`u8`)      | header byte                             |
| Max parity per message           | 128             | RS coder limit                          |
| Max audio per message            | 55 845 B        | 255 × 219                                |
| Min chunk size                   | 16 B            | per-frame overhead floor                |
| Stream sequence wrap             | 256             | per-(from,channel)                      |
| Confidentiality                  | delegated to Meshtastic channel AES-CTR | no per-message envelope at this layer |
| Header integrity                 | SHA-256[..4] (unkeyed) | catches bit-flips against AES-CTR malleability |
| FEC                              | RS over GF(2⁸)  | survives any `parity_count` losses      |
| Max NACK rounds                  | 32 cumulative  | bounds total airtime per message        |
| Codec                            | application-decided | protocol carries opaque bytes        |

The protocol explicitly does **not** provide:

- a built-in audio codec (out of scope; bring-your-own)
- end-to-end authentication or confidentiality beyond what Meshtastic's
  channel encryption already gives you. Any peer with the channel PSK
  can forge any frame on this layer (DATA, PARITY, NACK including
  `give_up`). A future revision MAY reintroduce a per-message AEAD
  envelope or a keyed header MAC if a concrete threat model warrants it
  — see git history (V2) for the previous design.
- congestion control (relies on adaptive pacing + per-sender rate limit)
- ordering across messages (only within a stream via `stream_seq`)

---

## Appendix A: Reference constants

```rust
pub const PROTOCOL_VERSION: u8 = 0x03;
pub const HEADER_SIZE: usize = 16;
pub const MAX_PACKET_SIZE: usize = 231;
pub const MAX_BODY_SIZE: usize = MAX_PACKET_SIZE - HEADER_SIZE;  // 215
pub const MAX_MESSAGE_BYTES: usize = MAX_CHUNKS_PER_MESSAGE * MAX_BODY_SIZE;  // 55_845
pub const MIN_CHUNK_SIZE: usize = 16;
pub const MAX_CHUNKS_PER_MESSAGE: usize = 255;
pub const MAX_PARITY_PER_MESSAGE: usize = 128;
pub const MAX_IN_PROGRESS_GLOBAL: usize = 64;
pub const MAX_IN_PROGRESS_PER_SENDER: usize = 4;
pub const BLACKLIST_TTL: Duration = Duration::from_secs(600);
pub const BLACKLIST_MAX: usize = 100;
pub const NACK_MAX_ROUNDS: u16 = 400;
pub const NACK_WINDOW_MS: u64 = 3000;
```

## Appendix B: Type/flag bit layout

```
   type_flags byte:
     bit 7  bit 6  bit 5      bit 4           bit 3      bits 2..0
     ┌──────────────┐ ┌────────┐ ┌──────────────┐ ┌────────┐ ┌──────────┐
     │ packet_type  │ │reserved│ │last_in_stream│ │reserved│ │ reserved │
     └──────────────┘ └────────┘ └──────────────┘ └────────┘ └──────────┘
       (2 bits)       (=0; V2    (1 bit)         (=0; V2    (3 bits, =0)
                      encrypted)                  mac_keyed)
```

| Field            | Mask   |
|------------------|--------|
| `packet_type`    | `0xC0` |
| reserved (V2 `encrypted`) | `0x20` |
| `last_in_stream` | `0x10` |
| reserved (V2 `mac_keyed`) | `0x08` |
| reserved         | `0x07` |
