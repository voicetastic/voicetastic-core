# Overview

[← Home](Home.md)

The Voicetastic Voice Protocol layers short voice messages on top of the
Meshtastic mesh. It runs as a tenant of `PortNum::PRIVATE_APP` (256),
fragments codec frames into LoRa-sized chunks, recovers loss with
Reed-Solomon FEC and selective NACKs, and optionally adds an end-to-end
AES-256-GCM envelope on top of Meshtastic's channel encryption.

---

## What it does

- Carries **pre-encoded codec frames** (AMR-NB, Opus, …) — never raw audio.
- **Fragments** an audio message into ≤ 219-byte chunks (LoRa MTU minus
  header).
- **Forward Error Correction** (Reed-Solomon over GF(2⁸)) tolerates loss up
  to `parity_count` chunks without retransmission.
- **Selective NACKs** with bitmap recover heavier loss in one round-trip.
- **End-to-end AES-256-GCM envelope** binds each message to its channel +
  sender, on top of Meshtastic's per-hop AES-CTR.
- **Resource-bounded** receiver: per-sender + global in-flight caps,
  blacklist for recently-finalized messages, validation-strike eviction
  for chatty bad senders.

## What it does *not* do

- No built-in audio codec — bring your own encoder/decoder.
- No per-recipient end-to-end encryption — the envelope is per-channel.
- No congestion control beyond adaptive pacing per modem preset.
- No cross-message ordering — only within a stream via `stream_seq`.
- No authenticated NACKs — see [Reliability](Reliability-FEC-and-NACK.md#nack-trust-model).

## Design priorities

1. **Survive packet loss** without round-trip-bound recovery whenever
   possible (FEC).
2. **Bounded airtime** — every message has a hard NACK-round cap and an
   absolute timeout.
3. **Codec-agnostic** wire format so codec evolution doesn't bump the
   protocol version.
4. **Resist abuse** — bound per-sender resource use, reject malformed
   frames at the earliest possible point, authenticate payloads end-to-end.
5. **Forward-compatible** — a single version byte at offset 0 lets future
   revisions coexist on the same port.

## When *not* to use it

- Real-time interactive voice. The protocol is **store-and-forward**:
  recordings of a few seconds, transmitted asynchronously. Latency is
  dominated by airtime + pacing, which on `LONG_SLOW` can exceed the
  duration of the recording itself.
- Anything requiring guaranteed delivery. The protocol gives up after
  `NACK_MAX_ROUNDS = 400` consecutive rounds without progress.
- High-fidelity audio. The MTU + airtime budget caps usable bitrates to
  the 5–16 kbps range.

---

## Where it fits

```
┌────────────────────────────────────────────────────┐
│ Application (recording UI, playback, message UX)   │
├────────────────────────────────────────────────────┤
│ Voice protocol (this document)                     │
│   • build_message  • VoiceAssembler  • NACK loop   │
├────────────────────────────────────────────────────┤
│ Meshtastic application layer                       │
│   • MeshPacket  • PortNum::PRIVATE_APP             │
├────────────────────────────────────────────────────┤
│ Meshtastic channel crypto (AES-256-CTR, per-PSK)   │
├────────────────────────────────────────────────────┤
│ LoRa PHY (modem preset chosen by the user)         │
└────────────────────────────────────────────────────┘
```

The voice protocol is one of many `PRIVATE_APP` tenants. The leading
version byte (`0x02`) lets receivers triage frames before parsing.

→ Continue to [Frame Format](Frame-Format.md).
