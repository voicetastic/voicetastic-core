# Voicetastic Voice Protocol — Wiki

Practical, navigable documentation for the **Voicetastic Voice Protocol** —
voice messaging over the [Meshtastic](https://meshtastic.org) mesh.

The normative wire-format spec lives in
[`VOICE_PROTOCOL.md`](../../VOICE_PROTOCOL.md). This wiki is the
implementer-friendly companion: it explains the *why*, walks through frames
byte-by-byte, and provides recipes for senders and receivers.

> **Protocol version: 2 (wire byte `0x02`)** • **Reference impl:**
> [`crates/voicetastic-core/src/voice/`](../../crates/voicetastic-core/src/voice/)

---

## Pages

| Page                                                          | Purpose                                                  |
|---------------------------------------------------------------|----------------------------------------------------------|
| [Overview](Overview.md)                                       | What the protocol does, design goals, non-goals.         |
| [Frame Format](Frame-Format.md)                               | Byte-level walkthrough of header + body for every type.  |
| [Reliability — FEC and NACK](Reliability-FEC-and-NACK.md)     | How loss recovery works, end-to-end.                     |
| [Encryption Envelope](Encryption.md)                          | AES-256-GCM keying, AAD, replay protection.              |
| [Sender Guide](Sender-Guide.md)                               | How to build a compatible transmitter.                   |
| [Receiver Guide](Receiver-Guide.md)                           | How to build a compatible reassembler.                   |
| [Constants and Limits](Constants-and-Limits.md)               | All numeric ceilings in one place, with rationale.       |
| [Error Catalogue](Error-Catalogue.md)                         | Every `VoiceError` variant and when it fires.            |
| [Settings](Settings.md)                                       | Client-side persisted settings (codec, bitrate, …).      |
| [Glossary](Glossary.md)                                       | Term definitions; read first if jargon trips you up.     |

---

## Quick start

1. **Pick a codec.** The protocol carries opaque bytes; any narrowband codec
   works. AMR-NB is the reference choice (see [Sender Guide](Sender-Guide.md)).
2. **Encode your audio.** Strip codec container headers (e.g. `#!AMR\n`) — the
   wire only carries raw codec frames.
3. **Send.** Build a [`VoiceSender`](../../crates/voicetastic-core/src/voice/sender.rs)
   once per `MeshService`, then call
   [`VoiceSender::send`](../../crates/voicetastic-core/src/voice/sender.rs)
   with a [`SendRequest`](../../crates/voicetastic-core/src/voice/sender.rs).
   The sender owns build → burst → NACK → retransmit → linger; consume
   `SendStatus` events from the returned handle.
4. **Receive.** On the other side, feed each PRIVATE_APP payload to
   [`VoiceAssembler::accept`](../../crates/voicetastic-core/src/voice/assembler/mod.rs);
   call `tick()` every ~100 ms to drive timeouts and NACKs.

---

## Status

- ✅ Builder, assembler, FEC, encryption envelope, NACK construction & parsing.
- ✅ Receiver-driven NACK transmission.
- ✅ Sender-side state machine: [`VoiceSender`](../../crates/voicetastic-core/src/voice/sender.rs)
  owns build → register → burst → NACK → retransmit → linger as a single
  shared pipeline; CLI / GUI / Android frontends just submit a
  `SendRequest` and consume `SendStatus` events. See the
  [Sender Guide](Sender-Guide.md).

See [`TODO.md`](../../TODO.md) for the wider roadmap.
