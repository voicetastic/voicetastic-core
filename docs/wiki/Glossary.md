# Glossary

[← Home](Home.md)

| Term                       | Definition                                                                                                                                       |
|----------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------|
| **AAD**                    | Additional Authenticated Data in AES-GCM. The voice protocol uses the 12 logical header bytes (`header[0..12]`, excluding the 4-byte MAC trailer) as AAD so any header tampering fails tag verification. |
| **AES-256-GCM**            | Authenticated symmetric cipher used for the optional end-to-end envelope. 96-bit nonce, 128-bit tag.                                              |
| **AMR-NB**                 | Adaptive Multi-Rate Narrowband. The reference codec; 8 bitrates from 4.75 to 12.2 kbps, 20 ms frame.                                              |
| **`channel`**              | Meshtastic channel index (`u32`). Determines the channel PSK used for both Meshtastic AES-CTR and the voice envelope HKDF salt.                   |
| **`channel_psk`**          | Pre-shared key configured in Meshtastic for a given channel. Used as the HKDF salt for envelope key derivation.                                   |
| **`chunk_index`**          | Index of a frame within its packet-type bucket. `0..total_data` for DATA, `0..parity_count` for PARITY.                                           |
| **`chunk_size`**           | Fixed body length for non-final DATA chunks and all PARITY chunks within a single message. Inferred by the receiver, not on the wire.             |
| **`codec`**                | One-byte advisory marker indicating which codec produced the bytes. Receivers MUST drop unknown codecs.                                            |
| **`codec_param`**          | Codec-specific metadata (e.g. AMR-NB bitrate ordinal). Pass-through; not range-checked by the protocol.                                            |
| **DATA frame**             | A frame whose body is the codec bytes for a single chunk of original audio.                                                                       |
| **`encrypted` bit**        | `type_flags` bit 5. When set, the body is `nonce ‖ ciphertext ‖ tag`.                                                                             |
| **FEC**                    | Forward Error Correction. Reed-Solomon over GF(2⁸); see [Reliability](Reliability-FEC-and-NACK.md).                                              |
| **`from`**                 | Sender's Meshtastic node id, formatted as lowercase `!hex8` (e.g. `!a1b2c3d4`). Strictly required for encrypted frames.                           |
| **`give_up`**              | NACK flag: the receiver has timed out. Sender SHOULD discard remaining queued chunks for that message_id.                                         |
| **HKDF-SHA256**            | RFC 5869 key derivation. Used to derive the envelope AES key from the channel PSK plus message-specific IKM.                                      |
| **`last_in_stream`**       | `type_flags` bit 4. Marks the final frame of a recording session. Currently informational in the reference receiver.                              |
| **`message_id`**           | Non-zero `u32` chosen by the sender. With `from`, uniquely identifies a message for ~60 s (BLACKLIST_TTL).                                        |
| **MTU**                    | Maximum Transmission Unit. Meshtastic LoRa MTU = 231 bytes (= `MAX_PACKET_SIZE`).                                                                 |
| **NACK frame**             | Negative ACK with bitmap of missing DATA chunk indices. See [Frame Format](Frame-Format.md#nack-frames).                                          |
| **`nack_window_ms`**       | Quiet period (default 1500 ms) after the last seen chunk before the receiver issues a NACK round.                                                  |
| **`nack_rounds`**          | Per-message counter of consecutive NACK rounds without progress; reset whenever a new shard lands, capped at `NACK_MAX_ROUNDS = 32`.            |
| **PARITY frame**           | A frame whose body is a Reed-Solomon parity shard. Always `chunk_size` bytes.                                                                     |
| **`packet_type`**          | Top 2 bits of `type_flags`: 0 = DATA, 1 = PARITY, 2 = NACK, 3 reserved.                                                                           |
| **`parity_count`**         | Number of FEC parity shards in a message. 0 disables FEC; max 128.                                                                                |
| **`PRIVATE_APP`**          | Meshtastic `PortNum` value 256. Voice frames ride here.                                                                                           |
| **PSK**                    | Pre-Shared Key. Meshtastic channel-level secret; salt for HKDF.                                                                                   |
| **Reed-Solomon**           | The FEC code used. `(total_data, parity_count)` shards over GF(2⁸); recovery from any `total_data` shards.                                        |
| **`stream_seq`**           | Per-`(from, channel)` monotonic `u8`, wrapping at 256. Intended for ordering overlapping recordings; informational in the reference receiver.    |
| **Template**               | The header fields locked in by the first frame of a message: `total_data`, `codec`, `stream_seq`, plus `chunk_size` once observable.              |
| **`total_data`**           | Number of original DATA chunks in a message. `u8`, must be ≥ 1.                                                                                   |
| **`type_flags`**           | Header byte 1, packing `packet_type` (2b), `encrypted` (1b), `last_in_stream` (1b), reserved (4b, must be 0).                                     |
| **`want_ack`**             | Meshtastic packet flag. Voice senders set it for DM DATA / PARITY frames; cleared for broadcasts and NACKs.                                       |
| **Blacklist**              | The receiver's recently-finalized `(from, message_id)` set. Late frames for finalized messages are silently dropped (TTL 60 s, max 100 entries).   |
| **Validation strike**      | Counter on each in-progress entry; incremented on post-template mismatches. After 3 strikes the entry is evicted + blacklisted.                   |

[← Home](Home.md)
