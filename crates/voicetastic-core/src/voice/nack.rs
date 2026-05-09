//! NACK (selective-retransmit) frame encode/decode.

use super::consts::HEADER_SIZE;
use super::error::{Result, VoiceError};
use super::header::ChunkHeader;
use super::types::{PacketType, VoiceCodec};

/// Build a NACK frame for `(from, message_id)` reporting `missing` data
/// chunk indices.
#[allow(clippy::too_many_arguments)]
pub fn build_nack(
    message_id: u32,
    stream_seq: u8,
    codec: VoiceCodec,
    codec_param: u8,
    total_data: u8,
    parity_count: u8,
    missing: &[u8],
    give_up: bool,
) -> Vec<u8> {
    let bitmap_len = (total_data as usize).div_ceil(8);
    let mut body = Vec::with_capacity(2 + bitmap_len);
    body.push(0x01); // nack_version
    body.push(if give_up { 0x01 } else { 0x00 });
    body.extend(std::iter::repeat_n(0u8, bitmap_len));
    for &idx in missing {
        if idx >= total_data {
            continue;
        }
        let byte = 2 + (idx as usize) / 8;
        let bit = 7 - ((idx as usize) % 8);
        body[byte] |= 1 << bit;
    }
    let header = ChunkHeader {
        packet_type: PacketType::Nack,
        encrypted: false,
        last_in_stream: false,
        message_id,
        codec,
        codec_param,
        stream_seq,
        chunk_index: 0,
        total_data,
        parity_count,
    };
    let mut frame = Vec::with_capacity(HEADER_SIZE + body.len());
    let mut hb = [0u8; HEADER_SIZE];
    header.write_into(&mut hb);
    frame.extend_from_slice(&hb);
    frame.extend_from_slice(&body);
    frame
}

/// Parsed NACK contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NackInfo {
    pub message_id: u32,
    pub stream_seq: u8,
    pub total_data: u8,
    pub parity_count: u8,
    pub give_up: bool,
    pub missing: Vec<u8>,
}

/// Decode a NACK frame's body.
pub fn parse_nack_body(header: &ChunkHeader, body: &[u8]) -> Result<NackInfo> {
    let bitmap_len = (header.total_data as usize).div_ceil(8);
    if body.len() < 2 + bitmap_len {
        return Err(VoiceError::NackTooShort);
    }
    if body[0] != 0x01 {
        return Err(VoiceError::BadVersion(body[0]));
    }
    let give_up = body[1] & 0x01 != 0;
    let mut missing = Vec::new();
    for i in 0..header.total_data {
        let byte = 2 + (i as usize) / 8;
        let bit = 7 - ((i as usize) % 8);
        if body[byte] & (1 << bit) != 0 {
            missing.push(i);
        }
    }
    Ok(NackInfo {
        message_id: header.message_id,
        stream_seq: header.stream_seq,
        total_data: header.total_data,
        parity_count: header.parity_count,
        give_up,
        missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nack_roundtrip() {
        let frame = build_nack(0x1234, 0, VoiceCodec::Opus, 16, 10, 2, &[1, 4, 9], false);
        let (h, body) = ChunkHeader::parse(&frame).unwrap();
        assert_eq!(h.packet_type, PacketType::Nack);
        let info = parse_nack_body(&h, body).unwrap();
        assert_eq!(info.missing, vec![1, 4, 9]);
        assert!(!info.give_up);
    }
}
