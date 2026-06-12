//! Cross-implementation wire-format golden vectors.
//!
//! These bytes are the **contract** every Voicetastic implementation must
//! reproduce on the wire - in particular the firmware's C++ `VtProtocol` /
//! `VtChunker` / Reed-Solomon. This test locks `voicetastic-proto` (the
//! normative impl) to `tests/wire_vectors.txt`; the firmware vendors that same
//! file and asserts its C++ output matches it (see the firmware's native
//! test). If either side ever diverges, one of the two tests fails - drift is
//! caught at PR time instead of as silent on-air incompatibility.
//!
//! Vectors are deterministic (no RNG: `message_id` is fixed in each case).
//!
//! To intentionally change the wire format: update the code, regenerate with
//!     VT_UPDATE_VECTORS=1 cargo test -p voicetastic-proto --test wire_vectors
//! then copy `wire_vectors.txt` to the firmware's vendored copy in the same PR.

use reed_solomon_erasure::galois_8::ReedSolomon;
use voicetastic_proto::builder::{BuildConfig, build_message};
use voicetastic_proto::header::ChunkHeader;
use voicetastic_proto::types::{PacketType, VoiceCodec};

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// (label, serialized wire bytes) for every locked case.
fn vectors() -> Vec<(&'static str, Vec<u8>)> {
    let mut out: Vec<(&'static str, Vec<u8>)> = Vec::new();

    // --- raw header serialization (field packing + 4-byte SHA-256 MAC) ---
    let headers: [(&str, ChunkHeader); 3] = [
        (
            "header/data-codec2",
            ChunkHeader {
                packet_type: PacketType::Data,
                last_in_stream: false,
                message_id: 0xDEAD_BEEF,
                codec: VoiceCodec::Codec2,
                codec_param: 5,
                stream_seq: 7,
                chunk_index: 3,
                total_data: 22,
                parity_count: 5,
            },
        ),
        (
            "header/parity-last",
            ChunkHeader {
                packet_type: PacketType::Parity,
                last_in_stream: true,
                message_id: 0x0001_0203,
                codec: VoiceCodec::Codec2,
                codec_param: 0,
                stream_seq: 255,
                chunk_index: 0,
                total_data: 1,
                parity_count: 0,
            },
        ),
        (
            "header/nack",
            ChunkHeader {
                packet_type: PacketType::Nack,
                last_in_stream: false,
                message_id: 0xFFFF_FFFF,
                codec: VoiceCodec::AmrNb,
                codec_param: 7,
                stream_seq: 42,
                chunk_index: 128,
                total_data: 200,
                parity_count: 64,
            },
        ),
    ];
    for (label, h) in headers {
        out.push((label, h.serialize().to_vec()));
    }

    // --- full messages: chunker + header/MAC + Reed-Solomon framing ---
    // Concatenate all frames so the vector covers chunk boundaries, padding,
    // and parity layout. Audio is a deterministic ramp.
    let ramp: Vec<u8> = (0..200u32).map(|i| (i & 0xff) as u8).collect();
    let msg_cases: [(&str, &[u8], BuildConfig); 3] = [
        (
            "msg/64b-chunk32-fec0",
            &ramp[..64],
            BuildConfig {
                message_id: 0xCAFE_BABE,
                stream_seq: 1,
                codec: VoiceCodec::Codec2,
                codec_param: 5,
                chunk_size: 32,
                parity_count: 0,
                last_in_stream: true,
            },
        ),
        (
            "msg/64b-chunk32-fec2",
            &ramp[..64],
            BuildConfig {
                message_id: 0xCAFE_BABE,
                stream_seq: 2,
                codec: VoiceCodec::Codec2,
                codec_param: 5,
                chunk_size: 32,
                parity_count: 2,
                last_in_stream: true,
            },
        ),
        (
            "msg/200b-chunk48-fec3",
            &ramp[..],
            BuildConfig {
                message_id: 0x1234_5678,
                stream_seq: 9,
                codec: VoiceCodec::Codec2,
                codec_param: 3,
                chunk_size: 48,
                parity_count: 3,
                last_in_stream: false,
            },
        ),
    ];
    for (label, audio, cfg) in msg_cases {
        let enc = build_message(audio, &cfg).expect("build_message");
        let mut bytes = Vec::new();
        for f in &enc.frames {
            bytes.extend_from_slice(f);
        }
        out.push((label, bytes));
    }

    // --- raw Reed-Solomon parity (locks the FEC layer byte-for-byte) ---
    // Encode a deterministic ramp split into shards, exactly as build_message
    // does (zero-padded parity shards, then ReedSolomon::encode), and golden
    // the resulting parity bytes. The firmware's rs::encode (vendored shard-RS
    // over GF(2^8), generator alpha=2, poly 0x11d) must reproduce these - that
    // is the scariest drift to catch, since a mismatch silently breaks FEC
    // recovery across implementations.
    let rs_cases: [(&str, usize, usize, usize); 2] = [
        ("rs/2data-2parity-32", 2, 2, 32),
        ("rs/4data-3parity-16", 4, 3, 16),
    ];
    for (label, data_n, parity_n, shard_sz) in rs_cases {
        let mut shards: Vec<Vec<u8>> = (0..data_n)
            .map(|s| {
                (0..shard_sz)
                    .map(|i| ((s * shard_sz + i) & 0xff) as u8)
                    .collect()
            })
            .collect();
        for _ in 0..parity_n {
            shards.push(vec![0u8; shard_sz]);
        }
        let rs = ReedSolomon::new(data_n, parity_n).expect("ReedSolomon::new");
        rs.encode(&mut shards).expect("rs encode");
        let mut parity = Vec::new();
        for p in &shards[data_n..] {
            parity.extend_from_slice(p);
        }
        out.push((label, parity));
    }

    out
}

#[test]
fn wire_vectors_match_golden() {
    let rendered = vectors()
        .iter()
        .map(|(label, bytes)| format!("{label} {}", to_hex(bytes)))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wire_vectors.txt");

    if std::env::var_os("VT_UPDATE_VECTORS").is_some() {
        std::fs::write(path, &rendered).expect("write golden vectors");
        eprintln!("wrote golden vectors to {path}");
        return;
    }

    let golden = std::fs::read_to_string(path).unwrap_or_default();
    assert_eq!(
        rendered, golden,
        "\nwire format drifted from the golden vectors.\n\
         If this change is INTENTIONAL, regenerate with:\n  \
         VT_UPDATE_VECTORS=1 cargo test -p voicetastic-proto --test wire_vectors\n\
         then sync tests/wire_vectors.txt to the firmware's vendored copy in the same PR.\n"
    );
}

#[test]
fn rs_parity_is_recoverable() {
    // Confirms reed-solomon-erasure (default-features=false, as proto uses it)
    // produces FUNCTIONAL parity: encode 2+2, drop a data shard, reconstruct.
    use reed_solomon_erasure::galois_8::ReedSolomon;
    let sz = 32usize;
    let d0: Vec<u8> = (0..sz).map(|i| i as u8).collect();
    let d1: Vec<u8> = (0..sz).map(|i| (sz + i) as u8).collect();
    let mut shards: Vec<Vec<u8>> = vec![d0.clone(), d1.clone(), vec![0; sz], vec![0; sz]];
    let rs = ReedSolomon::new(2, 2).unwrap();
    rs.encode(&mut shards).unwrap();
    // drop data shard 0
    let mut opt: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
    opt[0] = None;
    rs.reconstruct(&mut opt).unwrap();
    assert_eq!(
        opt[0].as_ref().unwrap(),
        &d0,
        "RS failed to recover dropped data shard"
    );
}
