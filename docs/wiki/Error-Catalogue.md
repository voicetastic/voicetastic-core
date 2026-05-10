# Error Catalogue

[← Home](Home.md)

Every variant of
[`VoiceError`](../../crates/voicetastic-core/src/voice/error.rs), what
triggers it, and how a caller should handle it.

The assembler returns errors via `AssemblyEvent::Rejected(VoiceError)`;
the builder returns them as `Result<EncodedMessage, VoiceError>`.

---

## Header / structure errors (parse time)

| Variant                | Trigger                                                 | Handle                                  |
|------------------------|---------------------------------------------------------|-----------------------------------------|
| `TooShort`             | Frame shorter than `HEADER_SIZE = 12 B`.                | Drop. Probably a non-voice packet.      |
| `TooLarge`             | Frame larger than `MAX_PACKET_SIZE = 231 B`.            | Drop; sender is broken.                 |
| `BadVersion(b)`        | First byte ≠ `0x01`.                                    | Drop; future protocol version.          |
| `ReservedFlagSet(b)`   | Low nibble of `type_flags` is non-zero.                 | Drop; sender is broken.                 |
| `ReservedPacketType`   | `packet_type == 3`.                                     | Drop; reserved.                         |
| `ZeroMessageId`        | `message_id == 0`.                                      | Drop; spec forbids zero.                |
| `BadTotal(0)`          | `total_data == 0`.                                      | Drop; spec forbids.                     |
| `TooMuchParity(n)`     | `parity_count > 128`.                                   | Drop; FEC coder limit.                  |
| `BadIndex { idx, total }` | `chunk_index ≥ total_data` (DATA) or `≥ parity_count` (PARITY). | Drop. |

## Body / chunk-size errors

| Variant                               | Trigger                                                                | Handle                       |
|---------------------------------------|------------------------------------------------------------------------|------------------------------|
| `ChunkTooSmall(n)`                    | Builder: `chunk_size < MIN_CHUNK_SIZE`.                                | Sender bug.                  |
| `ChunkTooLarge { got, max }`          | Body length out of `MIN_CHUNK_SIZE..=MAX_BODY_SIZE` (assembler) or oversized chunk_size (builder). | Drop / sender bug. |
| `BodyLenMismatch { got, expected }`   | DATA / PARITY body length differs from established `chunk_size` (excluding final DATA, which may be shorter). | Drop. |
| `AudioTooLarge { bytes, max }`        | Builder: input audio exceeds `MAX_CHUNKS_PER_MESSAGE × chunk_size`.    | Sender splits or compresses. |

## Codec / template errors

| Variant                                  | Trigger                                                  | Handle                                |
|------------------------------------------|----------------------------------------------------------|---------------------------------------|
| `UnknownCodec(b)`                        | Codec byte is `3..=255`.                                 | Drop; surface "codec unsupported".    |
| `CodecMismatch { first, got }`           | Frame's codec ≠ first frame's codec.                     | Drop; counts a validation strike.     |
| `TotalMismatch { first, got }`           | Frame's `total_data` ≠ first frame's `total_data`.       | Drop; counts a validation strike.     |
| `StreamSeqMismatch { first, got }`       | Frame's `stream_seq` ≠ first frame's `stream_seq`.       | Drop; counts a validation strike.     |

After `MAX_VALIDATION_STRIKES = 3` such mismatches on the same in-progress
entry, the entry is evicted and blacklisted to free its per-sender slot.

## Encryption errors

| Variant                       | Trigger                                                             | Handle                       |
|-------------------------------|---------------------------------------------------------------------|------------------------------|
| `BadTag`                      | AES-GCM tag verification failed.                                    | Drop; possible tampering.    |
| `BodyTooShortForEnv(n)`       | Encrypted body shorter than `nonce + tag = 28 B`.                   | Drop; sender bug.            |
| `EncryptedNack`               | NACK frame has the encryption bit set.                              | Drop; spec forbids.          |
| `EncryptedNoPsk`              | Encrypted frame received but `AssemblerConfig.channel_psk` is `None`. | Configure PSK or drop.     |
| `BadFromForEncrypted(s)`      | Encrypted frame's `from` is not strict `!hex8`.                     | Drop; potential spoof.       |

## NACK errors

| Variant         | Trigger                                                  | Handle |
|-----------------|----------------------------------------------------------|--------|
| `NackTooShort`  | NACK body shorter than `2 + ⌈total_data/8⌉` bytes.       | Drop.  |

## FEC errors

| Variant     | Trigger                                                          | Handle                       |
|-------------|------------------------------------------------------------------|------------------------------|
| `Fec(s)`    | The Reed-Solomon coder returned an internal error.               | Sender bug or memory issue.  |

## Resource bounds

| Variant                | Trigger                                                                        | Handle                                |
|------------------------|--------------------------------------------------------------------------------|---------------------------------------|
| `Blacklisted`          | `(from, message_id)` is on the recently-completed blacklist.                   | Drop; expected for late late frames.  |
| `PerSenderCap(from)`   | New `message_id` from a sender already at `MAX_IN_PROGRESS_PER_SENDER` (= 4).  | Drop; rate-limit signal.              |

---

## Severity guide for receivers

- **Silent drop, expected**: `Blacklisted`, `Duplicate` (returned as the
  `AssemblyEvent`, not an error), `BadVersion`, `ReservedPacketType`.
- **Silent drop, log at debug**: any structural / template / chunk-size
  error.
- **Log at warn**: `BadTag`, `BadFromForEncrypted`, `EncryptedNoPsk`,
  `PerSenderCap` — these are signs of misconfiguration or attack.
- **Surface to UI / app**: `UnknownCodec` (the user may want to install a
  codec), repeated `BadTag` from the same `from` (possible PSK
  mismatch).

→ Continue to [Glossary](Glossary.md).
