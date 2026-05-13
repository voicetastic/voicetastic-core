//! End-to-end voice protocol integration tests: sender (`build_message`) →
//! lossy "wire" → receiver (`VoiceAssembler`) → NACK round-trip →
//! retransmit / FEC recovery.
//!
//! These complement the per-module unit tests under
//! `crates/voicetastic-core/src/voice/...` by exercising the whole stack
//! end-to-end. They live as an integration test (separate crate) so they
//! can't accidentally peek at `pub(super)` internals — they must drive
//! the protocol exclusively through the published API surface, the same
//! way the GUI / CLI / Android bridge do.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use voicetastic_core::voice::{
    AssemblerConfig, AssemblyEvent, BuildConfig, NackInfo, OutboundNack, PacketType,
    VoiceAssembler, VoiceCodec, VoiceDestination, build_message, parse_nack_body,
};
use voicetastic_core::voice::{ChunkHeader, EnvelopeKey, derive_key};

const FROM: &str = "!abcdef01";
const FROM_NODE: u32 = 0xABCD_EF01;
const CHANNEL: u32 = 0;

fn synth(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 7) & 0xff) as u8).collect()
}

fn cfg(chunk_size: usize, parity: u8, key: Option<EnvelopeKey>) -> BuildConfig {
    BuildConfig {
        message_id: 0xCAFE_BABE,
        stream_seq: 0,
        codec: VoiceCodec::Opus,
        codec_param: 0,
        chunk_size,
        parity_count: parity,
        last_in_stream: true,
        encryption: key,
        mac_key: None,
    }
}

/// Deliver every frame in order, no loss: receiver completes on the last
/// DATA chunk and produces exactly the input audio back.
#[test]
fn loopback_no_loss_completes_cleanly() {
    let audio = synth(64 * 6 + 13);
    let enc = build_message(&audio, &cfg(64, 0, None)).unwrap();
    let asm = VoiceAssembler::new(AssemblerConfig::default());

    let mut completed = None;
    for f in &enc.frames {
        match asm.accept(FROM, VoiceDestination::Broadcast, CHANNEL, f) {
            AssemblyEvent::Pending { .. } => {}
            AssemblyEvent::Complete(m) => completed = Some(m),
            e => panic!("unexpected event: {e:?}"),
        }
    }
    let m = completed.expect("message should complete");
    assert!(m.is_complete);
    assert_eq!(m.audio, audio);
    assert_eq!(m.recovered_via_fec, 0);
}

/// Drop a single DATA chunk; FEC parity should recover it without ever
/// needing a NACK round.
#[test]
fn loopback_fec_recovers_one_loss_without_nack() {
    let audio = synth(64 * 5);
    // 2 parity shards — enough to recover any single loss.
    let enc = build_message(&audio, &cfg(64, 2, None)).unwrap();
    let asm = VoiceAssembler::new(AssemblerConfig::default());

    let mut completed = None;
    let drop_idx = 3;
    for (i, f) in enc.frames.iter().enumerate() {
        if i == drop_idx {
            continue;
        }
        match asm.accept(FROM, VoiceDestination::Broadcast, CHANNEL, f) {
            AssemblyEvent::Pending { .. }
            | AssemblyEvent::Duplicate
            | AssemblyEvent::Rejected(_) => {}
            AssemblyEvent::Complete(m) => completed = Some(m),
            AssemblyEvent::Nack(_) => panic!("FEC should recover before NACK round"),
        }
    }
    let m = completed.expect("FEC should produce a complete message");
    assert!(m.is_complete);
    assert_eq!(m.recovered_via_fec, 1);
    assert_eq!(m.audio, audio);
}

/// NACK-driven retransmit round-trip:
///   1. Sender emits all frames; receiver "loses" two DATA chunks.
///   2. We force a NACK-window timeout on the receiver and drive `tick()`.
///   3. We parse the emitted NACK, look up the requested indices on the
///      sender side, retransmit those frames, and verify completion.
#[test]
fn loopback_nack_retransmit_completes_message() {
    let audio = synth(64 * 8);
    let enc = build_message(&audio, &cfg(64, 0, None)).unwrap();
    // Tight NACK window so a single forced gap triggers a round; long
    // message_timeout so the test doesn't race the hard timeout.
    let asm = VoiceAssembler::new(AssemblerConfig {
        nack_window: Duration::from_millis(10),
        message_timeout: Duration::from_secs(30),
        max_nack_rounds: 8,
        ..AssemblerConfig::default()
    });

    // Deliver everything except chunks 2 and 5.
    let lost: HashSet<usize> = [2, 5].into_iter().collect();
    let mut completed = None;
    for (i, f) in enc.frames.iter().enumerate() {
        if lost.contains(&i) {
            continue;
        }
        match asm.accept(FROM, VoiceDestination::Broadcast, CHANNEL, f) {
            AssemblyEvent::Pending { .. } => {}
            AssemblyEvent::Complete(m) => completed = Some(m),
            e => panic!("unexpected event during initial burst: {e:?}"),
        }
    }
    assert!(
        completed.is_none(),
        "should not complete with chunks missing"
    );

    // Force the receiver's last_chunk_at into the past so `tick` emits a
    // NACK without us having to actually sleep.
    std::thread::sleep(Duration::from_millis(20));
    let out = asm.tick();
    assert_eq!(out.nacks.len(), 1, "expected exactly one NACK frame");
    let OutboundNack { ref frame, .. } = out.nacks[0];

    // Parse the NACK as a receiver peer would.
    let (hdr, body) = ChunkHeader::parse(frame, None).expect("NACK header must parse");
    assert_eq!(hdr.packet_type, PacketType::Nack);
    let info: NackInfo = parse_nack_body(&hdr, body).expect("NACK body must parse");
    assert_eq!(info.message_id, 0xCAFE_BABE);
    let missing: HashSet<u8> = info.missing.iter().copied().collect();
    assert!(missing.contains(&2));
    assert!(missing.contains(&5));

    // Sender side: retransmit only the requested DATA chunks.
    for idx in info.missing.iter().copied() {
        let f = &enc.frames[idx as usize];
        match asm.accept(FROM, VoiceDestination::Broadcast, CHANNEL, f) {
            AssemblyEvent::Pending { .. } => {}
            AssemblyEvent::Complete(m) => completed = Some(m),
            e => panic!("unexpected event during retransmit: {e:?}"),
        }
    }
    let m = completed.expect("retransmit should complete the message");
    assert!(m.is_complete);
    assert_eq!(m.audio, audio);
}

/// Encrypted end-to-end: receiver derives the per-message key from the
/// channel PSK + `(message_id, from_node_num)` and decrypts every body.
#[test]
fn loopback_encrypted_message_roundtrip() {
    let psk = b"unit-test-channel-psk";
    let audio = synth(64 * 4);
    let key = derive_key(psk, 0xCAFE_BABE, FROM_NODE);
    let enc = build_message(&audio, &cfg(64, 0, Some(key))).unwrap();
    let asm = VoiceAssembler::new(AssemblerConfig {
        channel_psk: Some(psk.to_vec()),
        ..AssemblerConfig::default()
    });

    let mut completed = None;
    for f in &enc.frames {
        match asm.accept(FROM, VoiceDestination::Broadcast, CHANNEL, f) {
            AssemblyEvent::Pending { .. } => {}
            AssemblyEvent::Complete(m) => completed = Some(m),
            e => panic!("unexpected event: {e:?}"),
        }
    }
    let m = completed.expect("encrypted message should complete");
    assert!(m.is_complete);
    assert!(m.encrypted);
    assert_eq!(m.audio, audio);
}

/// Hard timeout path: receiver gets only half the message and `tick` is
/// driven until the configured `message_timeout` expires. With
/// `partial_play_on_timeout = true` the message is finalised as
/// partial; with it set to `false` it's silently discarded.
#[test]
fn loopback_partial_on_timeout() {
    let audio = synth(64 * 6);
    let enc = build_message(&audio, &cfg(64, 0, None)).unwrap();
    let asm = VoiceAssembler::new(AssemblerConfig {
        nack_window: Duration::from_secs(3600), // suppress NACK rounds
        message_timeout: Duration::from_millis(50),
        max_nack_rounds: 0,
        partial_play_on_timeout: true,
        ..AssemblerConfig::default()
    });

    // Deliver the first half only.
    for f in enc.frames.iter().take(3) {
        let _ = asm.accept(FROM, VoiceDestination::Broadcast, CHANNEL, f);
    }

    // Spin `tick()` until something finalises or the wall clock runs out.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut finalised = None;
    while Instant::now() < deadline {
        let out = asm.tick();
        if let Some(m) = out.finalized.into_iter().next() {
            finalised = Some(m);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let m = finalised.expect("timeout should produce a partial message");
    assert!(!m.is_complete);
    assert_eq!(m.received_data, 3);
    assert_eq!(m.total_data, enc.total_data);
}
