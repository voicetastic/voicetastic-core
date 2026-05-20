# Encryption

Voice protocol **v3** removes its own envelope encryption layer.
Confidentiality is provided exclusively by Meshtastic's channel
encryption: the firmware AES-256-CTR-encrypts the entire `Data` payload
(this protocol's header + body) using the channel PSK before handing
the frame to the LoRa PHY. Peers without the channel PSK cannot read
the bytes.

This means:

- There is **no per-message AEAD** at this layer (v2 had AES-256-GCM
  with an HKDF-derived per-message key; removed in v3 to save ~28 octets
  per chunk and simplify the surface).
- The 4-byte `mac_tag` trailing the header is unkeyed
  `SHA-256(header[0..12])[..4]`. It catches on-air bit-flips (AES-CTR is
  bit-flip malleable) but is **not** an authenticator — any peer with
  the channel PSK can forge a valid tag.
- A consequence: a malicious channel member can fabricate or alter any
  frame on this layer, including NACK frames with `give_up=true`. That
  matches Meshtastic's threat model for text messages on the same channel.

## What about end-to-end confidentiality between two peers?

Out of scope for this protocol. If you need it, layer your own AEAD on
top of the codec frames before handing them to `build_message`, or wait
for a future revision — git history (the v2 line) shows what a
per-message envelope looked like and can be revived if a concrete
threat model warrants it.

## Migration from v2

V2 wrapped each DATA/PARITY body with AES-256-GCM keyed by
`HKDF-SHA256(channel_psk, message_id ‖ from_node_num, "voicetastic/v2")`.
V3 drops the wrap entirely; the `encrypted` flag bit (`0x20`) and
`mac_keyed` flag bit (`0x08`) are now reserved-zero and v3 parsers
reject frames that set them. The wire-version byte bumps from `0x02`
to `0x03` so v2 ↔ v3 frames cleanly fail the version check rather than
producing silent corruption.

See [`VOICE_PROTOCOL.md`](../../VOICE_PROTOCOL.md) §7 for the
authoritative spec.
