# Voicetastic Voice Protocol — Wiki

Practical, navigable documentation for the **Voicetastic Voice Protocol** —
voice messaging over the [Meshtastic](https://meshtastic.org) mesh.

The normative wire-format spec lives in
[`VOICE_PROTOCOL.md`](../../VOICE_PROTOCOL.md). This wiki is the
implementer-friendly companion: it explains the *why*, walks through frames
byte-by-byte, and provides recipes for senders and receivers.

> **Protocol version: 1** • **Reference impl:**
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
3. **Build the message.** Call
   [`build_message`](../../crates/voicetastic-core/src/voice/builder.rs) with
   a [`BuildConfig`](../../crates/voicetastic-core/src/voice/builder.rs#L17)
   that picks `chunk_size`, `parity_count`, and (optionally) an AES-GCM
   envelope key.
4. **Send each frame.** Push every entry of `EncodedMessage.frames` through
   `MeshService::send_data` on `PortNum::PRIVATE_APP` (256), pacing per
   [`ModemPreset::pacing`](../../crates/voicetastic-core/src/voice/types.rs#L94).
5. **Receive.** On the other side, feed each PRIVATE_APP payload to
   [`VoiceAssembler::accept`](../../crates/voicetastic-core/src/voice/assembler.rs);
   call `tick()` every ~100 ms to drive timeouts and NACKs.

---

## Status

- ✅ Builder, assembler, FEC, encryption envelope, NACK construction & parsing.
- ✅ Receiver-driven NACK transmission (CLI listener forwards them).
- ⏳ Sender-side state machine that consumes inbound NACKs and retransmits
  selectively. Tracked as TODO; current senders do best-effort first transmission only.

See [`TODO.md`](../../TODO.md) for the wider roadmap.
