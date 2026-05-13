# Header MAC — key-scoping options

The v2 wire format adds a 4-byte MAC trailer to the chunk header
(`bytes[12..16]`), covering the 12 logical header bytes `bytes[0..12]`.
Two modes share the slot, distinguished by the `MAC_KEYED_FLAG`
(`0x08`) bit in the flags byte:

- **Unkeyed**: `SHA-256(header[0..12])[..4]` — integrity only. Catches
  on-air bit-flips and accidental misroutes; offers no protection
  against an attacker on the channel.
- **Keyed (option A, current default)**: `HMAC-SHA256(K, header[0..12])[..4]`
  with `K = channel_psk`. Authenticates the header to any peer that
  holds the channel PSK.

The current implementation (option **A**) uses the raw channel PSK as
the HMAC key. The two alternatives below are documented here as
improvement clues; both fix specific weaknesses of A at the cost of
extra bookkeeping.

---

## Option A — channel-PSK MAC (chosen)

`K = channel_psk` (32 bytes from Meshtastic channel config).

**Pros**
- Zero handshake. Any peer that joined the channel can verify
  immediately.
- Same trust model as the channel-level AES-GCM payload encryption
  already in use, so no new key-distribution surface.
- Symmetric: one tag covers both directions; works for broadcast.

**Cons**
- Anyone on the channel can forge a header MAC (it is a channel-wide
  shared secret, not a per-sender identity). Compromise of any channel
  member compromises authenticity for every sender on that channel.
- Tag is not bound to the message content beyond what the 12-byte
  header already commits to (codec, ids, indices, lengths). The
  payload is independently authenticated by the AES-GCM tag in the
  body envelope (when encryption is enabled).
- Re-keying the channel rotates the MAC key in lockstep with the
  payload key; cannot be rotated independently.

---

## Option B — per-sender derived key

`K = HKDF-SHA256(channel_psk, info = "voicetastic-hmac-v2" || from_node_num)`.

**Pros**
- Compromise of one sender's derived key does not let an attacker
  forge tags for a different `from_node_num` on the same channel.
- Receivers can cache the derived key per `from`, avoiding HKDF on
  every frame.
- Drop-in over A: only `mesh_service` / `assembler` need to thread
  `from_node_num` into MAC verification, the wire format does not
  change.

**Cons**
- Still vulnerable to a channel-PSK compromise: HKDF is reversible
  given the PSK, so the per-sender key offers compartmentalisation
  among channel members but no defense against an outside attacker
  who already has the PSK.
- The `from_node_num` is itself only weakly authenticated (carried in
  the Meshtastic envelope, not the voice header), so an attacker on
  the channel can impersonate any sender by picking that sender's
  `from` and deriving the matching key. Mitigation: bind the derived
  key to a value the attacker cannot trivially spoof, e.g. the node's
  long-lived public key if/when one is available.

---

## Option C — per-message envelope key reuse

`K = msg_key`, where `msg_key` is the per-message AEAD key already
derived for the AES-GCM body envelope
(`HKDF(channel_psk, info = message_id || from_node_num)`).

**Pros**
- Header tag is cryptographically bound to the specific message
  envelope: a header forged in isolation cannot be paired with any
  body the receiver will accept.
- Forward secrecy of header authenticity follows the body envelope:
  rotating message keys per message rotates header keys per message.
- No new key schedule — reuses an existing derivation.

**Cons**
- Receivers must know the `message_id` before they can verify the
  header MAC, but `message_id` lives in the header itself. The order
  becomes: parse plaintext header → derive `msg_key` → verify header
  MAC → process body. This is fine for normal operation but couples
  header verification to the body's key schedule and means NACK
  frames (which are not encrypted) need a separate MAC key.
- Encryption becomes effectively mandatory for header authenticity
  unless an explicit unkeyed-fallback path is kept (we already keep
  one in A).
- Slightly more CPU per frame (one HKDF per `(from, message_id)`
  pair); cacheable but more state than A or B.

---

## Recommendation

Stay on **A** until a concrete threat model demands the
compartmentalisation of **B** or the per-message binding of **C**.
If channel-level compromise becomes a live concern, **B** is the
cheapest upgrade because it preserves the wire format and only
changes the key fed to `mac::verify`.

The flag bit `MAC_KEYED_FLAG = 0x08` and the 4-byte tag width are
forward-compatible with all three options — switching schemes is a
local change to `voice::mac` plus the call sites that resolve the
key.
