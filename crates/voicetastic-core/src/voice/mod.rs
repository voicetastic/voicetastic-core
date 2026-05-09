//! Voice protocol — see [`VOICE_PROTOCOL.md`](../../../../VOICE_PROTOCOL.md)
//! at the repository root for the wire-format spec; this module is the
//! reference implementation.
//!
//! This module is **codec-free**: it ships, reassembles, and FEC-protects
//! opaque codec frame bytes. AMR-NB, Opus, etc. encoding/decoding and
//! audio I/O are out of scope — callers feed us pre-encoded bytes and
//! receive pre-encoded bytes back.
//!
//! # Wire dispatch
//!
//! Receivers reading a frame from `PortNum::PRIVATE_APP` MUST drop any
//! frame whose first byte is not [`PROTOCOL_VERSION`] (`0x01`). The
//! version byte exists so that future protocol revisions can coexist on
//! the same port.
//!
//! # Submodule layout
//!
//! - [`consts`] — protocol constants.
//! - [`types`] — `PacketType`, `VoiceCodec`, `VoiceDestination`, `ModemPreset`.
//! - [`error`] — `VoiceError` and the local `Result` alias.
//! - [`header`] — `ChunkHeader` (parse/serialize the 12-byte frame header).
//! - [`crypto`] — `EnvelopeKey`, `derive_key`, `encrypt_body`, `decrypt_body`.
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
pub mod crypto;
pub mod error;
pub mod header;
pub mod message;
pub mod nack;
pub mod types;

pub use assembler::{AssemblerConfig, OutboundNack, TickOutput, VoiceAssembler};
pub use builder::{BuildConfig, EncodedMessage, build_message, random_message_id};
pub use consts::{
    BLACKLIST_MAX, BLACKLIST_TTL, GCM_NONCE_LEN, GCM_TAG_LEN, HEADER_SIZE, MAX_BODY_SIZE,
    MAX_CHUNKS_PER_MESSAGE, MAX_IN_PROGRESS_GLOBAL, MAX_IN_PROGRESS_PER_SENDER, MAX_PACKET_SIZE,
    MAX_PARITY_PER_MESSAGE, MIN_CHUNK_SIZE, NACK_MAX_ROUNDS, NACK_WINDOW_MS, PROTOCOL_VERSION,
};
pub use crypto::{EnvelopeKey, decrypt_body, derive_key, encrypt_body};
pub use error::{Result, VoiceError};
pub use header::ChunkHeader;
pub use message::{AssemblyEvent, VoiceMessage};
pub use nack::{NackInfo, build_nack, parse_nack_body};
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
    //! End-to-end tests that span builder + assembler + crypto.
    use super::*;

    fn cfg(parity: u8, encrypt: bool) -> BuildConfig {
        BuildConfig {
            message_id: 0xCAFEBABE,
            stream_seq: 7,
            codec: VoiceCodec::Opus,
            codec_param: 16,
            chunk_size: 64,
            parity_count: parity,
            last_in_stream: false,
            encryption: if encrypt {
                Some(EnvelopeKey::from_bytes([0x42; 32]))
            } else {
                None
            },
        }
    }

    fn assembler(encrypt: bool) -> VoiceAssembler {
        VoiceAssembler::new(AssemblerConfig {
            channel_psk: if encrypt {
                Some(b"unit-test-psk".to_vec())
            } else {
                None
            },
            ..Default::default()
        })
    }

    fn synthesize(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i & 0xff) as u8).collect()
    }

    #[test]
    fn build_and_assemble_no_loss_no_fec() {
        let audio = synthesize(500);
        let enc = build_message(&audio, &cfg(0, false)).unwrap();
        assert_eq!(enc.total_data, 500_u32.div_ceil(64) as u8);
        assert_eq!(enc.parity_count, 0);

        let asm = assembler(false);
        let mut completed = None;
        for f in &enc.frames {
            match asm.accept("!00000001", VoiceDestination::Broadcast, 0, f) {
                AssemblyEvent::Pending => {}
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
        let enc = build_message(&audio, &cfg(2, false)).unwrap();
        let asm = assembler(false);
        let mut completed = None;
        // Drop data chunk index 2; deliver everything else. After completion
        // the key is blacklisted, so trailing parity frames come back as
        // Rejected — that's expected.
        for (i, f) in enc.frames.iter().enumerate() {
            if i == 2 {
                continue;
            }
            match asm.accept("!aa", VoiceDestination::Broadcast, 0, f) {
                AssemblyEvent::Pending | AssemblyEvent::Duplicate | AssemblyEvent::Rejected(_) => {}
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
        let enc = build_message(&audio, &cfg(2, false)).unwrap();
        assert_eq!(enc.frames.len(), 6);
        let asm = assembler(false);
        let mut completed = None;
        // Deliver: data 0, 1, 3 + parity 0 (skip data 2 + parity 1)
        let order = [0usize, 1, 3, 4];
        for &idx in &order {
            match asm.accept("!bb", VoiceDestination::Broadcast, 0, &enc.frames[idx]) {
                AssemblyEvent::Pending => {}
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
    fn build_assemble_with_encryption() {
        let audio = synthesize(200);
        let mut c = cfg(1, true);
        c.chunk_size = 64;
        let enc = build_message(&audio, &c).unwrap();
        let body_len = enc.frames[0].len() - HEADER_SIZE;
        assert!(body_len >= 64 + GCM_NONCE_LEN + GCM_TAG_LEN);

        let from_id_str = "!12345678";
        let asm = VoiceAssembler::new(AssemblerConfig {
            channel_psk: Some(b"unit-test-psk".to_vec()),
            ..Default::default()
        });
        // Sender used the test BuildConfig key directly; rebuild encrypted
        // frames with the receiver-derivable key for an end-to-end test.
        let real_key = derive_key(b"unit-test-psk", c.message_id, 0x12345678);
        let mut c2 = c.clone();
        c2.encryption = Some(real_key);
        let enc2 = build_message(&audio, &c2).unwrap();
        let mut completed = None;
        for f in &enc2.frames {
            match asm.accept(from_id_str, VoiceDestination::Broadcast, 0, f) {
                AssemblyEvent::Pending | AssemblyEvent::Duplicate | AssemblyEvent::Rejected(_) => {}
                AssemblyEvent::Complete(m) => completed = Some(m),
                AssemblyEvent::Nack(_) => panic!("unexpected NACK"),
            }
        }
        let m = completed.expect("complete");
        assert!(m.encrypted);
        assert_eq!(m.audio, audio);
    }

    #[test]
    fn detect_version_branch() {
        assert_eq!(detect_version(&[0x01, 0, 0]), Some(0x01));
        assert_eq!(detect_version(&[0x99, 0, 0]), Some(0x99));
        assert_eq!(detect_version(&[]), None);
    }
}
