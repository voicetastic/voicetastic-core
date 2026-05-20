# Header MAC — historical notes

The voice protocol's v2 wire format carried a keyed-MAC variant of the
4-byte header tag: `HMAC-SHA256(channel_psk, header[0..12])[..4]`,
selected by a `mac_keyed` flag bit (`0x08`). It was documented here
alongside two future alternatives (per-sender derived key, per-message
envelope key reuse).

**V3 removed the keyed-MAC variant.** The header tag is now always
unkeyed `SHA-256(header[0..12])[..4]`, and the `0x08` bit is reserved
zero — any frame setting it is rejected as `ReservedFlagSet`. The
rationale matches the broader v2 → v3 simplification (see
[Encryption](Encryption.md)): a keyed channel-wide HMAC offers no
protection against a channel insider (anyone with the PSK can forge),
which is the same threat model Meshtastic itself accepts for channel
text traffic. The 28-octet AES-GCM body envelope was the only thing
that actually authenticated the audio payload, and that's gone too —
so the keyed header MAC no longer had a useful counterpart.

The unkeyed SHA-256 tag stays because it still catches on-air bit-flips
(AES-CTR is malleable) and accidental in-frame corruption that slips
past LoRa FEC.

## Reviving authentication

If a future threat model warrants real authentication of voice traffic
(e.g. tamper-evident broadcasts on a public channel), the cleanest
path is to revive the v2 design at both layers:

- Per-message AEAD body envelope (AES-256-GCM with HKDF-derived key).
- Per-message keyed header MAC bound to the envelope key — the
  "option C" design previously documented here.

Git history at the v2 line carries the working code for both. Don't
re-introduce just the keyed header tag without an authenticated body —
authenticating the header to a malicious channel insider while leaving
the payload mutable buys very little.
