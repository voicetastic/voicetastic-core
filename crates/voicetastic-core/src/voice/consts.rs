//! Protocol-wide constants. See the [Voice-Protocol wiki page](https://github.com/voicetastic/voicetastic-core/wiki/Voice-Protocol) Appendix A.

use std::time::Duration;

/// On-wire version byte. v3 removes the envelope encryption layer
/// (confidentiality now relies on Meshtastic channel encryption) and the
/// keyed-MAC variant of the trailing header tag. The 4-byte tag is
/// always SHA-256 truncated — see [`super::mac`].
pub const PROTOCOL_VERSION: u8 = 0x03;
/// Fixed header length preceding every frame: 12 logical bytes +
/// [`HEADER_MAC_LEN`]-byte integrity tag.
pub const HEADER_SIZE: usize = 16;
/// Width of the trailing header MAC tag — unkeyed SHA-256 truncated.
/// See [`super::mac`].
pub const HEADER_MAC_LEN: usize = 4;
/// Maximum total frame size (header + body) — Meshtastic LoRa MTU.
pub const MAX_PACKET_SIZE: usize = 231;
/// Maximum body bytes per frame (`MAX_PACKET_SIZE - HEADER_SIZE`).
pub const MAX_BODY_SIZE: usize = MAX_PACKET_SIZE - HEADER_SIZE;
/// Minimum allowed `chunk_size`.
pub const MIN_CHUNK_SIZE: usize = 16;
/// Maximum data chunks per message (`total_data` field is `u8`).
pub const MAX_CHUNKS_PER_MESSAGE: usize = 255;
/// Hard receive-side cap on the un-FEC payload of a single message
/// (`MAX_CHUNKS_PER_MESSAGE * MAX_BODY_SIZE`). Frames pushing the assembler
/// past this are rejected.
pub const MAX_MESSAGE_BYTES: usize = MAX_CHUNKS_PER_MESSAGE * MAX_BODY_SIZE;
/// Maximum parity chunks per message (Reed-Solomon coder limit).
pub const MAX_PARITY_PER_MESSAGE: usize = 128;
/// Global cap on concurrent in-progress reassemblies.
pub const MAX_IN_PROGRESS_GLOBAL: usize = 64;
/// Per-sender cap on concurrent in-progress reassemblies.
pub const MAX_IN_PROGRESS_PER_SENDER: usize = 4;
/// Default for [`crate::voice::AssemblerConfig::completion_memory`]:
/// how long the receiver remembers that a given `(from, message_id)`
/// already completed, so late chunks still rattling out of the sender's
/// firmware queue (or arriving over a long retransmit tail) don't
/// resurrect a phantom partial reassembly. Set comfortably above the
/// realistic worst-case sender airtime — Long Slow on a near-MTU clip
/// can take 2–3 minutes — so the blacklist outlives the actual TX.
pub const BLACKLIST_TTL: Duration = Duration::from_secs(600);
/// Recently-completed blacklist max entries.
pub const BLACKLIST_MAX: usize = 100;
/// **Experimental (flood-control).** Heuristic safety valve, not a
/// wire-format value: tuned empirically and may change between releases
/// without a protocol-version bump. If no real data (data/parity chunks)
/// arrive within this window the sender is presumed dead and NACKs are
/// suppressed until the message timeout fires, so the receiver stops
/// flooding the channel with cap-multiplied NACKs aimed at a sender that
/// has dropped off the mesh.
pub const DEAD_SENDER_TIMEOUT: Duration = Duration::from_secs(120);

/// Maximum NACK rounds per message before the receiver gives up. Each
/// round fires after [`NACK_WINDOW_MS`] of silence, so this also bounds
/// how long a stalled message survives. Sized so the consecutive-silence
/// budget (`NACK_MAX_ROUNDS × NACK_WINDOW_MS`) reaches the default
/// `AssemblerConfig::message_timeout` of 600 s — i.e. the absolute
/// per-message timeout is the only practical ceiling. The previous
/// value of `32` (~48 s) tripped well before `message_timeout` and
/// produced spurious "partial: N/M chunks" finalizes on slow LoRa
/// presets where inter-chunk gaps can routinely exceed a few seconds.
pub const NACK_MAX_ROUNDS: u16 = 400;
/// Quiet-period after the last seen chunk before issuing a NACK.
/// Receiver uses 3× exponential backoff: 3s, 9s, 27s, 81s, 243s cap.
pub const NACK_WINDOW_MS: u64 = 3000;
