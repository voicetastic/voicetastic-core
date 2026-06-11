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
    AssemblerConfig, AssemblyEvent, BuildConfig, ChunkHeader, NackInfo, OutboundNack, PacketType,
    VoiceAssembler, VoiceCodec, VoiceDestination, build_message, parse_nack_body,
};

const FROM: &str = "!abcdef01";
const CHANNEL: u32 = 0;

fn synth(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 7) & 0xff) as u8).collect()
}

fn cfg(chunk_size: usize, parity: u8) -> BuildConfig {
    BuildConfig {
        message_id: 0xCAFE_BABE,
        stream_seq: 0,
        codec: VoiceCodec::Opus,
        codec_param: 0,
        chunk_size,
        parity_count: parity,
        last_in_stream: true,
    }
}

/// Deliver every frame in order, no loss: receiver completes on the last
/// DATA chunk and produces exactly the input audio back.
#[test]
fn loopback_no_loss_completes_cleanly() {
    let audio = synth(64 * 6 + 13);
    let enc = build_message(&audio, &cfg(64, 0)).unwrap();
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
    let enc = build_message(&audio, &cfg(64, 2)).unwrap();
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

/// Regression for the RS `data + parity <= 256` limit: a long broadcast
/// recording with Auto FEC (50 % parity) used to fail `build_message`
/// outright once `total_data` passed ~171. With parity clamped against the
/// shard-sum limit it now builds, and FEC still recovers dropped chunks.
#[test]
fn loopback_heavy_fec_near_max_chunks_roundtrips() {
    use voicetastic_core::settings::VoiceFecMode;

    // ~200 data chunks at the minimum chunk size.
    let chunk_size = 16;
    let total_data = 200usize;
    let audio = synth(chunk_size * total_data);
    let parity = VoiceFecMode::Auto.resolve(true, None, total_data);
    assert!(
        total_data + parity as usize <= 256,
        "resolve must keep the shard sum within the RS limit"
    );

    let enc = build_message(&audio, &cfg(chunk_size, parity)).expect("should build with FEC");
    let asm = VoiceAssembler::new(AssemblerConfig::default());

    // Drop a handful of interior data frames; FEC has plenty of parity to
    // recover. (The final data chunk, index 199, is left intact: a missing
    // final chunk with unknown trimmed length deliberately defers FEC.)
    let lost: HashSet<usize> = [3, 7, 50, 120, 198].into_iter().collect();
    let mut completed = None;
    for (i, f) in enc.frames.iter().enumerate() {
        if lost.contains(&i) {
            continue;
        }
        match asm.accept(FROM, VoiceDestination::Broadcast, CHANNEL, f) {
            AssemblyEvent::Pending { .. }
            | AssemblyEvent::Duplicate
            | AssemblyEvent::Rejected(_) => {}
            AssemblyEvent::Complete(m) => completed = Some(m),
            AssemblyEvent::Nack(_) => panic!("broadcast must not NACK"),
        }
    }
    let m = completed.expect("FEC should complete the message");
    assert!(m.is_complete);
    assert_eq!(m.audio, audio);
}

/// NACK-driven retransmit round-trip.
#[test]
fn loopback_nack_retransmit_completes_message() {
    let audio = synth(64 * 8);
    let enc = build_message(&audio, &cfg(64, 0)).unwrap();
    let asm = VoiceAssembler::new(AssemblerConfig {
        nack_window: Duration::from_millis(10),
        message_timeout: Duration::from_secs(30),
        max_nack_rounds: 8,
        ..AssemblerConfig::default()
    });

    // Unicast: broadcasts suppress NACK emission by design, so the
    // retransmit round-trip path can only be exercised on a DM.
    let dest = VoiceDestination::Node(voicetastic_core::node::NodeId::from_u32(0xABCD_EF01));
    let lost: HashSet<usize> = [2, 5].into_iter().collect();
    let mut completed = None;
    for (i, f) in enc.frames.iter().enumerate() {
        if lost.contains(&i) {
            continue;
        }
        match asm.accept(FROM, dest, CHANNEL, f) {
            AssemblyEvent::Pending { .. } => {}
            AssemblyEvent::Complete(m) => completed = Some(m),
            e => panic!("unexpected event during initial burst: {e:?}"),
        }
    }
    assert!(
        completed.is_none(),
        "should not complete with chunks missing"
    );

    std::thread::sleep(Duration::from_millis(20));
    let out = asm.tick();
    assert_eq!(out.nacks.len(), 1, "expected exactly one NACK frame");
    let OutboundNack { ref frame, .. } = out.nacks[0];

    let (hdr, body) = ChunkHeader::parse(frame).expect("NACK header must parse");
    assert_eq!(hdr.packet_type, PacketType::Nack);
    let info: NackInfo = parse_nack_body(&hdr, body).expect("NACK body must parse");
    assert_eq!(info.message_id, 0xCAFE_BABE);
    let missing: HashSet<u8> = info.missing.iter().copied().collect();
    assert!(missing.contains(&2));
    assert!(missing.contains(&5));

    for idx in info.missing.iter().copied() {
        let f = &enc.frames[idx as usize];
        match asm.accept(FROM, dest, CHANNEL, f) {
            AssemblyEvent::Pending { .. } => {}
            AssemblyEvent::Complete(m) => completed = Some(m),
            e => panic!("unexpected event during retransmit: {e:?}"),
        }
    }
    let m = completed.expect("retransmit should complete the message");
    assert!(m.is_complete);
    assert_eq!(m.audio, audio);
}

/// Hard timeout path: receiver gets only half the message and `tick` is
/// driven until the configured `message_timeout` expires.
#[test]
fn loopback_partial_on_timeout() {
    let audio = synth(64 * 6);
    let enc = build_message(&audio, &cfg(64, 0)).unwrap();
    let asm = VoiceAssembler::new(AssemblerConfig {
        nack_window: Duration::from_secs(3600), // suppress NACK rounds
        message_timeout: Duration::from_millis(50),
        max_nack_rounds: 0,
        partial_play_on_timeout: true,
        ..AssemblerConfig::default()
    });

    for f in enc.frames.iter().take(3) {
        let _ = asm.accept(FROM, VoiceDestination::Broadcast, CHANNEL, f);
    }

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
