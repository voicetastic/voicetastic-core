# Encryption Envelope

[← Home](Home.md)

The voice protocol layers an **end-to-end AES-256-GCM** envelope on top of
Meshtastic's per-hop AES-256-CTR channel encryption. Use of the envelope
is OPTIONAL but strongly RECOMMENDED whenever a channel PSK is available.

The envelope is signaled by the `encrypted` bit (bit 5 of `type_flags`).

---

## Threat model

Meshtastic's channel AES-CTR protects:

- ✅ Confidentiality on the LoRa air interface against off-channel listeners.

It does **not** protect against:

- ❌ Other members of the same channel (they share the PSK).
- ❌ The BLE / serial link to the radio (frames are decrypted on the
  radio before reaching the host).
- ❌ Radio firmware / OS read access to in-flight data.

The voice envelope adds, on top:

- ✅ Confidentiality of the body all the way from sender host to receiver
  host (radios cannot read it).
- ✅ Authenticity binding to `(channel_psk, message_id, from)` —
  replaying a frame on a different channel or with a spoofed sender id
  fails the GCM tag check.
- ✅ Header tamper-detection: the 12 logical header bytes are
  authenticated as AAD by the body envelope when encryption is enabled,
  and unconditionally by the 4-byte trailing header MAC (see
  [Frame-Format](Frame-Format.md) and
  [Header-MAC-Future-Work](Header-MAC-Future-Work.md)) — the MAC trailer
  itself is **not** part of the AAD.

It does **not** provide:

- ❌ Per-recipient privacy (the envelope is per-channel — anyone with the
  PSK can derive the same key).
- ❌ NACK authenticity (NACKs are deliberately unenveloped — see
  [Reliability](Reliability-FEC-and-NACK.md#nack-trust-model)).

---

## Key derivation

```
key = HKDF-SHA256(
    salt = channel_psk,
    ikm  = message_id_be ‖ from_node_num_be,
    info = "voicetastic/v2"
)
```

Sizes: `key` = 32 B (AES-256). `ikm` is exactly 8 bytes (4 + 4, big-endian).

The HKDF `info` string is the literal `"voicetastic/v2"`. It does **not**
track the wire-protocol version byte — it is preserved across protocol
revisions for forward-compat with already-shipped derivers, and is
permanent.

### Why this shape?

- Tying the key to `message_id` means **fresh nonce space per message**:
  the random 96-bit nonce per frame has zero collision risk within the
  255-frame budget of a single message_id.
- Tying the key to `from_node_num` means **spoofed sender ⇒ wrong key**.
  An attacker who replays a captured frame under a different `from`
  cannot produce a valid tag.
- Using HKDF with the PSK as salt (not IKM) follows
  [RFC 5869 §3.1](https://www.rfc-editor.org/rfc/rfc5869) recommendations
  for low-entropy or known-context salts.

---

## Per-frame envelope

```
                ┌── 12 B ──┐ ┌─ 12 B ─┐ ┌──────── ciphertext ────────┐ ┌─ 16 B ─┐
encrypted=1:    │  header  │ │ nonce  │ │   AES-256-GCM(plaintext)   │ │  tag   │
                └─────┬────┘ └────────┘ └────────────────────────────┘ └────────┘
                      │              ▲                                       ▲
                      └── AAD ───────┘                                       │
                                                  │                          │
                            ciphertext + tag ◄────┴── written together by ───┘
                                                     AES-GCM Encrypt
```

- **Nonce** — 96 bits (12 bytes) from the OS RNG, *prepended* to the body.
- **AAD** — the **12 logical header bytes** (offset 0..12, i.e. the
  header *without* its 4-byte MAC trailer). Tamper any
  header byte and the tag verification fails.
- **Tag** — 128 bits (16 bytes) appended after the ciphertext, per the
  AES-GCM standard.

### Body length math

With encryption enabled:

```
chunk_size_max = MAX_BODY_SIZE − GCM_NONCE_LEN − GCM_TAG_LEN
              = 219 − 12 − 16
              = 191 bytes
```

So encrypted messages can carry at most `255 × 191 = 48 705 bytes` of
audio versus `55 845 bytes` plaintext.

---

## Required receiver checks

When the `encrypted` bit is set, the receiver MUST:

1. **Have a channel PSK configured.** If not, drop with
   [`VoiceError::EncryptedNoPsk`](../../crates/voicetastic-core/src/voice/error.rs).
2. **Parse `from` strictly** as the lowercase `!hex8` Meshtastic node id.
   A malformed `from` produces a different (failing) key, but the spec
   requires explicit rejection via
   [`VoiceError::BadFromForEncrypted`](../../crates/voicetastic-core/src/voice/error.rs).
3. **Re-derive the key** per the formula above.
4. **Verify the GCM tag** against the original 12 logical header bytes as
   AAD. Tag failure ⇒ silent drop with
   [`VoiceError::BadTag`](../../crates/voicetastic-core/src/voice/error.rs).

---

## NACKs

NACK frames MUST have `encrypted = 0`. Receivers MUST drop a NACK with
the encryption bit set
([`VoiceError::EncryptedNack`](../../crates/voicetastic-core/src/voice/error.rs)).

This is intentional — see the
[NACK trust model](Reliability-FEC-and-NACK.md#nack-trust-model).

---

## Worked example

Sender:

```rust
use voicetastic_core::voice::{derive_key, BuildConfig, VoiceCodec, build_message};

let key = derive_key(channel_psk, message_id, my_node_num);
let cfg = BuildConfig {
    message_id,
    stream_seq,
    codec: VoiceCodec::AmrNb,
    codec_param: 5, // MR795
    chunk_size: 160,
    parity_count: 2,
    last_in_stream: true,
    encryption: Some(key),
};
let enc = build_message(&audio, &cfg)?;
// enc.frames is ready to push to MeshService::send_data
```

Receiver:

```rust
use voicetastic_core::voice::{AssemblerConfig, VoiceAssembler};

let asm = VoiceAssembler::new(AssemblerConfig {
    channel_psk: Some(channel_psk.to_vec()),
    ..Default::default()
});

// Then for every PRIVATE_APP frame received:
match asm.accept(&from_id, to, channel, &payload) {
    AssemblyEvent::Complete(msg) => save(&msg),
    AssemblyEvent::Pending | AssemblyEvent::Duplicate => {}
    AssemblyEvent::Nack(_) => {/* route to send-side */}
    AssemblyEvent::Rejected(e) => warn!(?e, "rejected"),
}
```

→ Continue to [Sender Guide](Sender-Guide.md).
