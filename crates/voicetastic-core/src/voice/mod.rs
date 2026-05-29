//! Voice protocol — see the [Voice-Protocol wiki page](https://git.cha-sam.re/voicetastic/voicetastic-desktop/-/wikis/Voice-Protocol)
//! for the wire-format spec; this module is the reference implementation.
//!
//! This module is **codec-free**: it ships, reassembles, and FEC-protects
//! opaque codec frame bytes. AMR-NB, Opus, etc. encoding/decoding and
//! audio I/O are out of scope — callers feed us pre-encoded bytes and
//! receive pre-encoded bytes back.
//!
//! # Wire dispatch
//!
//! Receivers reading a frame from `PortNum::PRIVATE_APP` MUST drop any
//! frame whose first byte is not [`PROTOCOL_VERSION`] (`0x03`). The
//! version byte exists so that future protocol revisions can coexist on
//! the same port; V3 is wire-incompatible with V2 (which carried an
//! AES-GCM body envelope and an optional keyed-MAC variant of the header
//! tag — both removed in favour of Meshtastic's channel encryption).
//!
//! # Submodule layout
//!
//! - [`consts`] — protocol constants.
//! - [`types`] — `PacketType`, `VoiceCodec`, `VoiceDestination`, `ModemPreset`.
//! - [`error`] — `VoiceError` and the local `Result` alias.
//! - [`header`] — `ChunkHeader` (parse/serialize the 16-byte frame header).
//! - [`mac`] — 4-byte trailing header integrity tag (SHA-256 truncated).
//! - [`builder`] — `BuildConfig`, `EncodedMessage`, `build_message`,
//!   `random_message_id`.
//! - [`nack`] — `build_nack`, `NackInfo`, `parse_nack_body`.
//! - [`message`] — `VoiceMessage`, `AssemblyEvent`.
//! - [`assembler`] — `VoiceAssembler`, `AssemblerConfig`, `TickOutput`,
//!   `OutboundNack`.

#![allow(clippy::result_large_err)]

pub mod assembler;
pub mod builder;
pub mod consts;
pub mod error;
pub mod header;
pub mod mac;
pub mod message;
pub mod nack;
pub mod outgoing;
pub mod send_prep;
pub mod sender;
pub mod sink;
pub mod tx_policy;
pub mod tx_state;
pub mod types;

pub use assembler::{AssemblerConfig, OutboundNack, TickOutput, VoiceAssembler};
pub use builder::{BuildConfig, EncodedMessage, build_message, random_message_id};
pub use consts::{
    BLACKLIST_MAX, BLACKLIST_TTL, DEAD_SENDER_TIMEOUT, HEADER_MAC_LEN, HEADER_SIZE, MAX_BODY_SIZE,
    MAX_CHUNKS_PER_MESSAGE, MAX_IN_PROGRESS_GLOBAL, MAX_IN_PROGRESS_PER_SENDER, MAX_MESSAGE_BYTES,
    MAX_PACKET_SIZE, MAX_PARITY_PER_MESSAGE, MIN_CHUNK_SIZE, NACK_MAX_ROUNDS, NACK_WINDOW_MS,
    PROTOCOL_VERSION,
};
pub use error::{Result, VoiceError};
pub use header::ChunkHeader;
pub use message::{AssemblyEvent, VoiceMessage};
pub use nack::{NackInfo, build_nack, parse_nack_body};
pub use outgoing::{
    DEFAULT_RETAIN_TTL, MAX_RETRANSMITS_PER_MESSAGE, OutgoingVoice, OutgoingVoiceRegistry,
};
pub use send_prep::{PreparedVoice, prepare_voice_send};
pub use sender::{DEFAULT_LINGER, SendHandle, SendRequest, SendStatus, VoiceSender};
pub use tx_state::{VoiceTx, VoiceTxAction};
pub use types::{ModemPreset, PacketType, VoiceCodec, VoiceDestination};

/// Returns the protocol version byte of a `PRIVATE_APP` payload.
///
/// Receivers should drop any frame whose first byte is not
/// [`PROTOCOL_VERSION`].
pub fn detect_version(bytes: &[u8]) -> Option<u8> {
    bytes.first().copied()
}

#[cfg(test)]
mod tests {
    //! End-to-end tests that span builder + assembler.
    use super::*;

    fn cfg(parity: u8) -> BuildConfig {
        BuildConfig {
            message_id: 0xCAFEBABE,
            stream_seq: 7,
            codec: VoiceCodec::Opus,
            codec_param: 16,
            chunk_size: 64,
            parity_count: parity,
            last_in_stream: false,
        }
    }

    fn assembler() -> VoiceAssembler {
        VoiceAssembler::new(AssemblerConfig::default())
    }

    fn synthesize(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i & 0xff) as u8).collect()
    }

    #[test]
    fn build_and_assemble_no_loss_no_fec() {
        let audio = synthesize(500);
        let enc = build_message(&audio, &cfg(0)).unwrap();
        assert_eq!(enc.total_data, 500_u32.div_ceil(64) as u8);
        assert_eq!(enc.parity_count, 0);

        let asm = assembler();
        let mut completed = None;
        for f in &enc.frames {
            match asm.accept("!00000001", VoiceDestination::Broadcast, 0, f) {
                AssemblyEvent::Pending { .. } => {}
                AssemblyEvent::Complete(m) => completed = Some(m),
                e => panic!("unexpected: {e:?}"),
            }
        }
        let m = completed.expect("completion");
        assert!(m.is_complete);
        assert_eq!(m.audio, audio);
        assert_eq!(m.recovered_via_fec, 0);
    }

    #[test]
    fn fec_recovers_dropped_data_chunk() {
        let audio = synthesize(64 * 5);
        let enc = build_message(&audio, &cfg(2)).unwrap();
        let asm = assembler();
        let mut completed = None;
        // Drop data chunk index 2; deliver everything else. After completion
        // the key is blacklisted, so trailing parity frames come back as
        // Rejected — that's expected.
        for (i, f) in enc.frames.iter().enumerate() {
            if i == 2 {
                continue;
            }
            match asm.accept("!aa", VoiceDestination::Broadcast, 0, f) {
                AssemblyEvent::Pending { .. }
                | AssemblyEvent::Duplicate
                | AssemblyEvent::Rejected(_) => {}
                AssemblyEvent::Complete(m) => completed = Some(m),
                AssemblyEvent::Nack(_) => panic!("unexpected NACK"),
            }
        }
        let m = completed.expect("FEC should have recovered with 2 parity shards");
        assert!(m.is_complete);
        assert_eq!(m.recovered_via_fec, 1);
        assert_eq!(m.audio, audio);
    }

    #[test]
    fn fec_completes_with_one_loss_and_parity() {
        let audio = synthesize(64 * 4);
        let enc = build_message(&audio, &cfg(2)).unwrap();
        assert_eq!(enc.frames.len(), 6);
        let asm = assembler();
        let mut completed = None;
        // Deliver: data 0, 1, 3 + parity 0 (skip data 2 + parity 1)
        let order = [0usize, 1, 3, 4];
        for &idx in &order {
            match asm.accept("!bb", VoiceDestination::Broadcast, 0, &enc.frames[idx]) {
                AssemblyEvent::Pending { .. } => {}
                AssemblyEvent::Complete(m) => completed = Some(m),
                e => panic!("unexpected: {e:?}"),
            }
        }
        let m = completed.expect("FEC should have recovered with parity");
        assert!(m.is_complete);
        assert_eq!(m.recovered_via_fec, 1);
        assert_eq!(m.audio, audio);
    }

    #[test]
    fn detect_version_branch() {
        assert_eq!(detect_version(&[0x01, 0, 0]), Some(0x01));
        assert_eq!(detect_version(&[0x99, 0, 0]), Some(0x99));
        assert_eq!(detect_version(&[]), None);
    }

    /// Regression: if the first arriving frame is the (possibly trimmed)
    /// final DATA chunk, chunk_size discovery must be deferred until a
    /// non-final DATA or any PARITY frame arrives. Previously the receiver
    /// would freeze chunk_size to the trimmed length and reject every
    /// subsequent full-size frame as BodyLenMismatch.
    #[test]
    fn last_chunk_first_does_not_freeze_chunk_size() {
        let audio = synthesize(64 * 3 + 17);
        let enc = build_message(&audio, &cfg(0)).unwrap();
        assert_eq!(enc.total_data, 4);
        let asm = assembler();
        let mut order: Vec<usize> = (0..enc.frames.len()).collect();
        let last = order.remove(enc.total_data as usize - 1);
        order.insert(0, last);
        let mut completed = None;
        for idx in order {
            match asm.accept(
                "!cafebabe",
                VoiceDestination::Broadcast,
                0,
                &enc.frames[idx],
            ) {
                AssemblyEvent::Pending { .. } => {}
                AssemblyEvent::Complete(m) => completed = Some(m),
                e => panic!("unexpected: {e:?}"),
            }
        }
        let m = completed.expect("completion");
        assert_eq!(m.audio, audio);
    }

    /// Spec §3.2: receivers MUST drop frames with an unknown codec.
    #[test]
    fn unknown_codec_is_rejected() {
        // Build a valid frame, then poke the codec byte to an unknown
        // value and recompute the header MAC so the frame survives the
        // header integrity check and reaches codec validation.
        let audio = synthesize(64);
        let enc = build_message(&audio, &cfg(0)).unwrap();
        let mut frame = enc.frames[0].clone();
        frame[6] = 0xEE; // codec
        let tag = super::mac::compute_tag(&frame[..HEADER_SIZE - HEADER_MAC_LEN]);
        frame[HEADER_SIZE - HEADER_MAC_LEN..HEADER_SIZE].copy_from_slice(&tag);
        let asm = assembler();
        let ev = asm.accept("!cc", VoiceDestination::Broadcast, 0, &frame);
        assert!(
            matches!(ev, AssemblyEvent::Rejected(VoiceError::UnknownCodec(0xEE))),
            "expected UnknownCodec, got {ev:?}",
        );
    }

    /// Codec known to the wire (e.g. AMR-NB byte 0) but not in the
    /// receiver's `supported_codecs` allowlist is rejected with
    /// `UnsupportedCodec` *before* any reassembly state is allocated.
    #[test]
    fn unsupported_codec_is_rejected_when_allowlist_set() {
        let audio = synthesize(64 * 2);
        let enc = build_message(&audio, &cfg(0)).unwrap();
        let asm = VoiceAssembler::new(AssemblerConfig {
            supported_codecs: Some(vec![VoiceCodec::AmrNb]),
            ..Default::default()
        });
        let ev = asm.accept("!aa", VoiceDestination::Broadcast, 0, &enc.frames[0]);
        assert!(
            matches!(
                ev,
                AssemblyEvent::Rejected(VoiceError::UnsupportedCodec(VoiceCodec::Opus))
            ),
            "expected UnsupportedCodec(Opus), got {ev:?}",
        );
    }

    /// Regression: V2 frames (header version byte 0x02) must be rejected
    /// by a V3 parser. Combined with the reserved-bit checks in
    /// `header_rejects_v2_encrypted_bit` / `header_rejects_v2_keyed_mac_bit`
    /// this gives a clean break between the two protocol revisions.
    #[test]
    fn v2_frame_is_rejected() {
        let asm = assembler();
        // Build a buffer with version byte 0x02 and otherwise plausible
        // contents. The parser must reject on version *before* touching
        // the MAC.
        let mut frame = vec![0u8; HEADER_SIZE + 4];
        frame[0] = 0x02; // V2
        frame[2..6].copy_from_slice(&0xDEADBEEFu32.to_be_bytes());
        let ev = asm.accept("!ab", VoiceDestination::Broadcast, 0, &frame);
        assert!(
            matches!(ev, AssemblyEvent::Rejected(VoiceError::BadVersion(0x02))),
            "expected BadVersion(0x02), got {ev:?}"
        );
    }

    /// A duplicate DATA frame whose body length differs from the first
    /// arrival is reported as `Duplicate`, not `BodyLenMismatch` — the
    /// slot is already filled, so we don't leak that the original body
    /// length mattered to a probing attacker.
    #[test]
    fn tampered_duplicate_reports_duplicate_not_mismatch() {
        let audio = synthesize(64 * 3);
        let enc = build_message(&audio, &cfg(0)).unwrap();
        let asm = assembler();
        assert!(matches!(
            asm.accept("!aa", VoiceDestination::Broadcast, 0, &enc.frames[0]),
            AssemblyEvent::Pending { .. },
        ));
        let mut tampered = enc.frames[0].clone();
        tampered.truncate(HEADER_SIZE + 32);
        let ev = asm.accept("!aa", VoiceDestination::Broadcast, 0, &tampered);
        assert!(
            matches!(ev, AssemblyEvent::Duplicate),
            "expected Duplicate, got {ev:?}",
        );
    }

    /// Regression: once a `(from, message_id)` pair has completed, late
    /// chunks for the same id must NOT resurrect a fresh in-progress
    /// assembly within the configured `completion_memory` window.
    #[test]
    fn late_chunk_after_complete_does_not_resurrect_assembly() {
        let audio = synthesize(64 * 4);
        let enc = build_message(&audio, &cfg(0)).unwrap();
        assert_eq!(enc.total_data, 4);
        let asm = assembler();
        let from = "!deadbeef";

        let mut completed = None;
        for f in &enc.frames {
            match asm.accept(from, VoiceDestination::Broadcast, 0, f) {
                AssemblyEvent::Pending { .. } => {}
                AssemblyEvent::Complete(m) => completed = Some(m),
                e => panic!("unexpected: {e:?}"),
            }
        }
        assert!(completed.is_some_and(|m| m.is_complete));

        for f in &enc.frames {
            let ev = asm.accept(from, VoiceDestination::Broadcast, 0, f);
            assert!(
                !matches!(
                    ev,
                    AssemblyEvent::Pending { .. } | AssemblyEvent::Complete(_)
                ),
                "replayed frame after complete produced {ev:?}, expected a Rejected/Ignored variant",
            );
        }
    }

    /// Regression for finding #1: when the **last** DATA chunk is lost
    /// and we have enough parity to reconstruct it, the assembler must
    /// NOT silently emit a padded shard — its real (un-padded) length was
    /// never observed on the wire, so any trailing zeros would corrupt
    /// the audio tail.
    #[test]
    fn fec_does_not_pad_recovered_last_data() {
        let audio = synthesize(64 * 3 + 17);
        let enc = build_message(&audio, &cfg(2)).unwrap();
        assert_eq!(enc.total_data, 4);
        assert_eq!(enc.parity_count, 2);
        let asm = assembler();
        for i in [0usize, 1, 2, 4, 5] {
            match asm.accept("!ab", VoiceDestination::Broadcast, 0, &enc.frames[i]) {
                AssemblyEvent::Pending { .. } => {}
                e => panic!("unexpected event for idx {i}: {e:?}"),
            }
        }
        match asm.accept("!ab", VoiceDestination::Broadcast, 0, &enc.frames[3]) {
            AssemblyEvent::Complete(m) => {
                assert!(m.is_complete);
                assert_eq!(m.audio, audio, "audio must match exactly — no padding");
            }
            e => panic!("expected Complete on final DATA arrival, got {e:?}"),
        }
    }

    /// Regression for finding #7: a follow-up frame whose `parity_count`
    /// is **less than** the value established by the first frame is
    /// rejected as `ParityCountDecrease`.
    #[test]
    fn parity_count_decrease_is_rejected() {
        let audio = synthesize(64 * 3);
        let enc = build_message(&audio, &cfg(2)).unwrap();
        let asm = assembler();
        let _ = asm.accept("!cd", VoiceDestination::Broadcast, 0, &enc.frames[0]);
        // Parse, mutate, re-serialize so the MAC stays valid.
        let (mut hdr, body) = ChunkHeader::parse(&enc.frames[1]).expect("frame 1 must parse");
        hdr.parity_count = 1;
        let mut tampered = Vec::with_capacity(HEADER_SIZE + body.len());
        tampered.extend_from_slice(&hdr.serialize());
        tampered.extend_from_slice(body);
        let ev = asm.accept("!cd", VoiceDestination::Broadcast, 0, &tampered);
        assert!(
            matches!(
                ev,
                AssemblyEvent::Rejected(VoiceError::ParityCountDecrease { first: 2, got: 1 })
            ),
            "expected ParityCountDecrease, got {ev:?}",
        );
    }

    /// Spec §3.4: NACK frames carry `chunk_index = 0`. The header parser
    /// MUST reject any other value before the body is touched.
    #[test]
    fn nack_with_nonzero_chunk_index_is_rejected() {
        use super::types::PacketType;
        let h = ChunkHeader {
            packet_type: PacketType::Nack,
            last_in_stream: false,
            message_id: 0xCAFEBABE,
            codec: VoiceCodec::Opus,
            codec_param: 16,
            stream_seq: 0,
            chunk_index: 5,
            total_data: 4,
            parity_count: 0,
        };
        let mut frame = vec![0u8; HEADER_SIZE + 4];
        frame[..HEADER_SIZE].copy_from_slice(&h.serialize());
        frame[HEADER_SIZE..].copy_from_slice(&[0x01, 0x00, 0x00, 0x00]);
        let asm = assembler();
        let ev = asm.accept("!ef", VoiceDestination::Broadcast, 0, &frame);
        assert!(
            matches!(ev, AssemblyEvent::Rejected(VoiceError::BadNackIndex(5))),
            "expected BadNackIndex(5), got {ev:?}",
        );
    }
}
