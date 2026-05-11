//! Smoke tests for the UDL <-> Rust conversion layer.
//!
//! Functional correctness of the protocol itself lives in
//! `voicetastic-core::voice::tests`; this file only locks in that the
//! bridge wires bytes through faithfully.
#[cfg(test)]
mod tests {
    use super::super::*;
    fn cfg(parity: u8, psk: Option<Vec<u8>>) -> BuildConfig {
        BuildConfig {
            message_id: 0xCAFEBABE,
            stream_seq: 7,
            codec: VoiceCodec::Opus,
            codec_param: 16,
            chunk_size: 64,
            parity_count: parity,
            last_in_stream: false,
            channel_psk: psk,
            from_node_num: 0x12345678,
        }
    }
    #[test]
    fn build_and_assemble_roundtrip_plaintext() {
        let audio = (0u8..200).cycle().take(300).collect::<Vec<_>>();
        let encoded = build_message(audio.clone(), cfg(2, None)).unwrap();
        assert!(encoded.frames.len() >= encoded.total_data as usize);
        let asm = VoiceAssembler::new(AssemblerConfig {
            message_timeout_ms: 60_000,
            partial_play_on_timeout: false,
            channel_psk: None,
            max_nack_rounds: 8,
            nack_window_ms: 1_500,
            completion_memory_ms: 60_000,
        });
        let mut got_complete = false;
        for frame in encoded.frames.iter().take(encoded.total_data as usize) {
            let ev = asm.accept("!12345678".into(), true, 0, 0, frame.clone());
            if let AssemblyEvent::Complete { message } = ev {
                assert!(message.is_complete);
                assert_eq!(message.audio, audio);
                got_complete = true;
            }
        }
        assert!(got_complete, "assembler never reported Complete");
    }
    #[test]
    fn fec_recovers_dropped_chunk() {
        let audio = (0u8..200).cycle().take(300).collect::<Vec<_>>();
        let encoded = build_message(audio.clone(), cfg(3, None)).unwrap();
        let total_data = encoded.total_data as usize;
        let asm = VoiceAssembler::new(AssemblerConfig {
            message_timeout_ms: 60_000,
            partial_play_on_timeout: false,
            channel_psk: None,
            max_nack_rounds: 8,
            nack_window_ms: 1_500,
            completion_memory_ms: 60_000,
        });
        // Drop data chunk 1; feed remaining data + first parity shard.
        let mut sent = 0;
        for (i, frame) in encoded.frames.iter().enumerate() {
            if i == 1 {
                continue;
            }
            sent += 1;
            let ev = asm.accept("!12345678".into(), true, 0, 0, frame.clone());
            if let AssemblyEvent::Complete { message } = ev {
                assert!(message.is_complete);
                assert!(message.recovered_via_fec >= 1);
                assert_eq!(message.audio, audio);
                return;
            }
            if sent >= total_data {
                break;
            }
        }
        panic!("FEC did not recover the missing chunk");
    }
    #[test]
    fn nack_roundtrip() {
        let frame = build_nack(NackConfig {
            message_id: 0x1234,
            stream_seq: 0,
            codec: VoiceCodec::Opus,
            codec_param: 16,
            total_data: 10,
            parity_count: 2,
            missing: vec![1, 4, 9],
            give_up: false,
        });
        let asm = VoiceAssembler::new(AssemblerConfig {
            message_timeout_ms: 60_000,
            partial_play_on_timeout: false,
            channel_psk: None,
            max_nack_rounds: 8,
            nack_window_ms: 1_500,
            completion_memory_ms: 60_000,
        });
        let ev = asm.accept("!12345678".into(), true, 0, 0, frame);
        match ev {
            AssemblyEvent::Nack { info } => {
                assert_eq!(info.missing, vec![1, 4, 9]);
                assert!(!info.give_up);
            }
            other => panic!("expected Nack, got {:?}", other),
        }
    }
    #[test]
    fn detect_version_passthrough() {
        assert_eq!(detect_version(vec![0x01, 2, 3]), Some(0x01));
        assert_eq!(detect_version(vec![]), None);
    }
    #[test]
    fn random_message_id_is_nonzero() {
        for _ in 0..16 {
            assert_ne!(random_message_id(), 0);
        }
    }
}
