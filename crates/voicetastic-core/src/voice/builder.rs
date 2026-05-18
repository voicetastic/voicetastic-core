//! Sender-side message chunker / Reed-Solomon encoder / framer.

use reed_solomon_erasure::galois_8::ReedSolomon;

use super::consts::{
    GCM_NONCE_LEN, GCM_TAG_LEN, HEADER_SIZE, MAX_BODY_SIZE, MAX_CHUNKS_PER_MESSAGE,
    MAX_PACKET_SIZE, MAX_PARITY_PER_MESSAGE, MIN_CHUNK_SIZE,
};
use super::crypto::{EnvelopeKey, encrypt_body};
use super::error::{Result, VoiceError};
use super::header::ChunkHeader;
use super::types::{PacketType, VoiceCodec};

/// Configuration for [`build_message`].
#[derive(Debug, Clone)]
pub struct BuildConfig {
    pub message_id: u32,
    pub stream_seq: u8,
    pub codec: VoiceCodec,
    pub codec_param: u8,
    pub chunk_size: usize,
    pub parity_count: u8,
    pub last_in_stream: bool,
    /// If `Some`, every DATA/PARITY body is wrapped with the AES-GCM
    /// envelope. `None` keeps bodies plaintext.
    pub encryption: Option<EnvelopeKey>,
    /// Channel PSK for the trailing 4-byte header MAC. `Some` ⇒
    /// HMAC-SHA256 (authenticity); `None` ⇒ unkeyed SHA-256
    /// (integrity only). Independent of `encryption`: a sender on a
    /// PSK-configured channel SHOULD pass the PSK here even for
    /// plaintext frames so the receiver can reject tampered headers.
    pub mac_key: Option<Vec<u8>>,
}

/// Output of [`build_message`]: the wire frames to transmit, in send order.
#[derive(Debug)]
pub struct EncodedMessage {
    /// Ready-to-send packet bodies (header + body), each ≤ [`MAX_PACKET_SIZE`].
    pub frames: Vec<Vec<u8>>,
    /// Number of original data chunks (frames `[0..total_data]`).
    pub total_data: u8,
    /// Number of parity chunks (frames `[total_data..]`).
    pub parity_count: u8,
}

/// Generate a non-zero random `u32` suitable for use as a `message_id`.
///
/// Fails with [`VoiceError::Rng`] if the OS RNG is unreachable. Callers
/// that want a panic-on-failure semantics can `.expect()` the result; the
/// fallible signature exists so sandboxed / seccomp'd hosts surface a
/// clean error instead of aborting the process.
pub fn random_message_id() -> Result<u32> {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).map_err(|e| VoiceError::Rng(e.to_string()))?;
    Ok(u32::from_be_bytes(buf).max(1))
}

/// Chunk `audio` (codec frame bytes — no container header) into frames.
///
/// Steps (see spec §8):
/// 1. Split `audio` into `total_data` chunks of `chunk_size` bytes (last
///    chunk may be shorter; padded with zeros for FEC, padding stripped on
///    reassembly).
/// 2. RS-encode `parity_count` parity shards (skipped if `parity_count == 0`).
/// 3. For each shard: build header, optionally encrypt body, push frame.
pub fn build_message(audio: &[u8], cfg: &BuildConfig) -> Result<EncodedMessage> {
    if cfg.chunk_size < MIN_CHUNK_SIZE {
        return Err(VoiceError::ChunkTooSmall(cfg.chunk_size));
    }
    if cfg.chunk_size > MAX_BODY_SIZE
        || (cfg.encryption.is_some()
            && cfg.chunk_size + GCM_NONCE_LEN + GCM_TAG_LEN > MAX_BODY_SIZE)
    {
        return Err(VoiceError::ChunkTooLarge {
            got: cfg.chunk_size,
            max: if cfg.encryption.is_some() {
                MAX_BODY_SIZE - GCM_NONCE_LEN - GCM_TAG_LEN
            } else {
                MAX_BODY_SIZE
            },
        });
    }
    if audio.is_empty() {
        return Err(VoiceError::TooShort { len: 0, needed: 1 });
    }
    let total_data_usize = audio.len().div_ceil(cfg.chunk_size);
    if total_data_usize > MAX_CHUNKS_PER_MESSAGE {
        return Err(VoiceError::AudioTooLarge {
            bytes: audio.len(),
            max: MAX_CHUNKS_PER_MESSAGE * cfg.chunk_size,
        });
    }
    if cfg.parity_count as usize > MAX_PARITY_PER_MESSAGE {
        return Err(VoiceError::TooMuchParity(cfg.parity_count));
    }

    let total_data = total_data_usize as u8;
    let parity_count = cfg.parity_count;

    // Build padded data shards (last shard zero-padded for FEC math).
    let mut shards: Vec<Vec<u8>> = Vec::with_capacity(total_data_usize + parity_count as usize);
    for chunk in audio.chunks(cfg.chunk_size) {
        let mut s = vec![0u8; cfg.chunk_size];
        s[..chunk.len()].copy_from_slice(chunk);
        shards.push(s);
    }
    // Real (un-padded) length of the last data chunk — needed to drop padding
    // on reassembly. The receiver derives this from the body length of the
    // final DATA frame, so we need to send it un-padded on the wire while
    // keeping the padded copy for FEC encoding.
    let last_data_len = audio.len() - cfg.chunk_size * (total_data_usize - 1);

    // RS-encode parity (zero-fill new parity shards, then encode in place).
    if parity_count > 0 {
        for _ in 0..parity_count {
            shards.push(vec![0u8; cfg.chunk_size]);
        }
        let rs = ReedSolomon::new(total_data_usize, parity_count as usize)
            .map_err(|e| VoiceError::Fec(e.to_string()))?;
        rs.encode(&mut shards)
            .map_err(|e| VoiceError::Fec(e.to_string()))?;
    }

    // Frame each shard.
    let mut frames: Vec<Vec<u8>> = Vec::with_capacity(shards.len());
    for (idx, shard) in shards.iter().enumerate() {
        let (packet_type, chunk_index, body_plain) = if (idx as u8) < total_data {
            // Data frame: trim padding on the last chunk.
            let body = if idx == total_data_usize - 1 {
                &shard[..last_data_len]
            } else {
                shard.as_slice()
            };
            (PacketType::Data, idx as u8, body)
        } else {
            (
                PacketType::Parity,
                (idx - total_data_usize) as u8,
                shard.as_slice(),
            )
        };
        let header = ChunkHeader {
            packet_type,
            encrypted: cfg.encryption.is_some(),
            // last_in_stream marks the very last frame of the very last
            // message in a recording session.
            last_in_stream: cfg.last_in_stream && idx == shards.len() - 1,
            message_id: cfg.message_id,
            codec: cfg.codec,
            codec_param: cfg.codec_param,
            stream_seq: cfg.stream_seq,
            chunk_index,
            total_data,
            parity_count,
            // Overwritten by serialize_with_mac to mirror mac_key.
            mac_keyed: false,
        };
        let mut header = header;
        let header_bytes = header.serialize_with_mac(cfg.mac_key.as_deref());
        let body = match &cfg.encryption {
            Some(key) => encrypt_body(key, &header_bytes, body_plain)?,
            None => body_plain.to_vec(),
        };
        if HEADER_SIZE + body.len() > MAX_PACKET_SIZE {
            return Err(VoiceError::ChunkTooLarge {
                got: body.len(),
                max: MAX_BODY_SIZE,
            });
        }
        let mut frame = Vec::with_capacity(HEADER_SIZE + body.len());
        frame.extend_from_slice(&header_bytes);
        frame.extend_from_slice(&body);
        frames.push(frame);
    }

    Ok(EncodedMessage {
        frames,
        total_data,
        parity_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> BuildConfig {
        BuildConfig {
            message_id: 12345,
            stream_seq: 0,
            codec: VoiceCodec::AmrNb,
            codec_param: 5,
            chunk_size: 64,
            parity_count: 0,
            last_in_stream: true,
            encryption: None,
            mac_key: None,
        }
    }

    #[test]
    fn random_message_id_never_zero() {
        for _ in 0..100 {
            let id = random_message_id().expect("RNG should work");
            assert_ne!(id, 0, "message_id should never be zero");
        }
    }

    #[test]
    fn build_single_chunk_no_fec() {
        let audio = vec![42u8; 32];
        let cfg = base_config();
        let msg = build_message(&audio, &cfg).expect("should build");

        assert_eq!(msg.total_data, 1);
        assert_eq!(msg.parity_count, 0);
        assert_eq!(msg.frames.len(), 1);
    }

    #[test]
    fn build_multiple_chunks_no_fec() {
        let audio = vec![42u8; 200];
        let cfg = base_config();
        let msg = build_message(&audio, &cfg).expect("should build");

        assert_eq!(msg.total_data, 4);
        assert_eq!(msg.parity_count, 0);
        assert_eq!(msg.frames.len(), 4);
    }

    #[test]
    fn build_with_parity() {
        let audio = vec![42u8; 128];
        let mut cfg = base_config();
        cfg.parity_count = 2;
        let msg = build_message(&audio, &cfg).expect("should build");

        assert_eq!(msg.total_data, 2);
        assert_eq!(msg.parity_count, 2);
        assert_eq!(msg.frames.len(), 4);
    }

    #[test]
    fn error_chunk_too_small() {
        let audio = vec![42u8; 32];
        let mut cfg = base_config();
        cfg.chunk_size = 15;
        let err = build_message(&audio, &cfg).expect_err("should fail");

        match err {
            VoiceError::ChunkTooSmall(sz) => assert_eq!(sz, 15),
            _ => panic!("unexpected error: {err:?}"),
        }
    }

    #[test]
    fn error_chunk_too_large() {
        let audio = vec![42u8; 32];
        let mut cfg = base_config();
        cfg.chunk_size = MAX_BODY_SIZE + 1;
        let err = build_message(&audio, &cfg).expect_err("should fail");

        match err {
            VoiceError::ChunkTooLarge { .. } => {}
            _ => panic!("unexpected error: {err:?}"),
        }
    }

    #[test]
    fn error_empty_audio() {
        let audio = vec![];
        let cfg = base_config();
        let err = build_message(&audio, &cfg).expect_err("should fail");

        match err {
            VoiceError::TooShort { .. } => {}
            _ => panic!("unexpected error: {err:?}"),
        }
    }

    #[test]
    fn error_audio_too_large() {
        let audio = vec![42u8; MAX_CHUNKS_PER_MESSAGE * MAX_BODY_SIZE + 1];
        let cfg = base_config();
        let err = build_message(&audio, &cfg).expect_err("should fail");

        match err {
            VoiceError::AudioTooLarge { .. } => {}
            _ => panic!("unexpected error: {err:?}"),
        }
    }

    #[test]
    fn error_parity_too_large() {
        let audio = vec![42u8; 32];
        let mut cfg = base_config();
        cfg.parity_count = (MAX_PARITY_PER_MESSAGE + 1) as u8;
        let err = build_message(&audio, &cfg).expect_err("should fail");

        match err {
            VoiceError::TooMuchParity(_) => {}
            _ => panic!("unexpected error: {err:?}"),
        }
    }

    #[test]
    fn frames_respect_max_packet_size() {
        let audio = vec![42u8; 128];
        let cfg = base_config();
        let msg = build_message(&audio, &cfg).expect("should build");

        for frame in &msg.frames {
            assert!(frame.len() <= MAX_PACKET_SIZE);
        }
    }

    #[test]
    fn frame_headers_correct() {
        let audio = vec![42u8; 200];
        let mut cfg = base_config();
        cfg.parity_count = 2;
        let msg = build_message(&audio, &cfg).expect("should build");

        for (i, frame) in msg.frames.iter().enumerate() {
            assert!(frame.len() >= HEADER_SIZE);

            let header_bytes = &frame[..HEADER_SIZE];
            let header = ChunkHeader::parse(header_bytes, None).expect("should parse");
            let (header, _) = header;

            assert_eq!(header.message_id, cfg.message_id);
            assert_eq!(header.codec, cfg.codec);
            assert_eq!(header.total_data, msg.total_data);
            assert_eq!(header.parity_count, msg.parity_count);

            if i < msg.total_data as usize {
                assert_eq!(header.packet_type, PacketType::Data);
                assert_eq!(header.chunk_index, i as u8);
            } else {
                assert_eq!(header.packet_type, PacketType::Parity);
                assert_eq!(header.chunk_index, (i - msg.total_data as usize) as u8);
            }
        }
    }

    #[test]
    fn last_chunk_trim() {
        let audio = vec![42u8; 150];
        let cfg = base_config();
        let msg = build_message(&audio, &cfg).expect("should build");

        let last_frame = &msg.frames[msg.frames.len() - 1];
        let body_len = last_frame.len() - HEADER_SIZE;

        assert_eq!(
            body_len,
            150 - (cfg.chunk_size * (msg.total_data as usize - 1))
        );
    }

    #[test]
    fn last_in_stream_flag() {
        let audio = vec![42u8; 32];
        let mut cfg = base_config();
        cfg.last_in_stream = true;
        let msg = build_message(&audio, &cfg).expect("should build");

        let last_frame = &msg.frames[msg.frames.len() - 1];
        let header = ChunkHeader::parse(&last_frame[..HEADER_SIZE], None)
            .expect("should parse")
            .0;
        assert!(header.last_in_stream);
    }

    #[test]
    fn max_chunks_per_message_limit() {
        let chunk_size = MAX_BODY_SIZE;
        let audio = vec![42u8; chunk_size * (MAX_CHUNKS_PER_MESSAGE - 1)];
        let mut cfg = base_config();
        cfg.chunk_size = chunk_size;
        let msg = build_message(&audio, &cfg).expect("should build");

        assert_eq!(msg.total_data as usize, MAX_CHUNKS_PER_MESSAGE - 1);
    }
}
