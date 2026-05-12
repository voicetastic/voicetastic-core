//! 12-byte chunk header parse/serialize.

use super::consts::{HEADER_SIZE, MAX_PACKET_SIZE, MAX_PARITY_PER_MESSAGE, PROTOCOL_VERSION};
use super::error::{Result, VoiceError};
use super::types::{PacketType, VoiceCodec};

/// Parsed view of a chunk header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHeader {
    pub packet_type: PacketType,
    pub encrypted: bool,
    pub last_in_stream: bool,
    pub message_id: u32,
    pub codec: VoiceCodec,
    pub codec_param: u8,
    pub stream_seq: u8,
    pub chunk_index: u8,
    pub total_data: u8,
    pub parity_count: u8,
}

impl ChunkHeader {
    fn type_flags_byte(&self) -> u8 {
        let mut b = self.packet_type.to_bits();
        if self.encrypted {
            b |= 0x20;
        }
        if self.last_in_stream {
            b |= 0x10;
        }
        b
    }

    /// Serialise to a fixed 12-byte buffer.
    ///
    /// Returning a `[u8; HEADER_SIZE]` instead of taking `&mut [u8]` removes
    /// the previous `assert!(out.len() >= HEADER_SIZE)` panic risk for any
    /// future caller that hands us a too-small buffer.
    pub(crate) fn serialize(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        out[0] = PROTOCOL_VERSION;
        out[1] = self.type_flags_byte();
        out[2..6].copy_from_slice(&self.message_id.to_be_bytes());
        out[6] = self.codec.to_byte();
        out[7] = self.codec_param;
        out[8] = self.stream_seq;
        out[9] = self.chunk_index;
        out[10] = self.total_data;
        out[11] = self.parity_count;
        out
    }

    /// Parse a frame header from `bytes`. Returns the header and the body
    /// slice. Validates version, packet type, and structural ranges.
    pub fn parse(bytes: &[u8]) -> Result<(Self, &[u8])> {
        if bytes.len() < HEADER_SIZE {
            return Err(VoiceError::TooShort {
                len: bytes.len(),
                needed: HEADER_SIZE,
            });
        }
        if bytes.len() > MAX_PACKET_SIZE {
            return Err(VoiceError::TooLarge {
                len: bytes.len(),
                max: MAX_PACKET_SIZE,
            });
        }
        if bytes[0] != PROTOCOL_VERSION {
            return Err(VoiceError::BadVersion(bytes[0]));
        }
        let tf = bytes[1];
        if tf & 0x0F != 0 {
            return Err(VoiceError::ReservedFlagSet(tf));
        }
        let packet_type = PacketType::from_bits(tf).ok_or(VoiceError::ReservedPacketType)?;
        let encrypted = tf & 0x20 != 0;
        let last_in_stream = tf & 0x10 != 0;
        let message_id = u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
        if message_id == 0 {
            return Err(VoiceError::ZeroMessageId);
        }
        let codec = VoiceCodec::from_byte(bytes[6]);
        let codec_param = bytes[7];
        let stream_seq = bytes[8];
        let chunk_index = bytes[9];
        let total_data = bytes[10];
        let parity_count = bytes[11];
        if total_data == 0 {
            return Err(VoiceError::BadTotal(total_data));
        }
        if parity_count as usize > MAX_PARITY_PER_MESSAGE {
            return Err(VoiceError::TooMuchParity(parity_count));
        }
        match packet_type {
            PacketType::Data => {
                if chunk_index >= total_data {
                    return Err(VoiceError::BadIndex {
                        idx: chunk_index,
                        total: total_data,
                    });
                }
            }
            PacketType::Parity => {
                if parity_count == 0 || chunk_index >= parity_count {
                    return Err(VoiceError::BadIndex {
                        idx: chunk_index,
                        total: parity_count,
                    });
                }
            }
            PacketType::Nack => {
                if encrypted {
                    return Err(VoiceError::EncryptedNack);
                }
                // Spec §3.4: NACK frames carry chunk_index = 0.
                if chunk_index != 0 {
                    return Err(VoiceError::BadNackIndex(chunk_index));
                }
            }
        }
        Ok((
            Self {
                packet_type,
                encrypted,
                last_in_stream,
                message_id,
                codec,
                codec_param,
                stream_seq,
                chunk_index,
                total_data,
                parity_count,
            },
            &bytes[HEADER_SIZE..],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::consts::HEADER_SIZE;
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = ChunkHeader {
            packet_type: PacketType::Data,
            encrypted: true,
            last_in_stream: true,
            message_id: 0x12345678,
            codec: VoiceCodec::Opus,
            codec_param: 16,
            stream_seq: 42,
            chunk_index: 3,
            total_data: 10,
            parity_count: 4,
        };
        let mut buf = vec![0u8; HEADER_SIZE + 5];
        buf[..HEADER_SIZE].copy_from_slice(&h.serialize());
        buf[HEADER_SIZE..].copy_from_slice(b"hello");
        let (parsed, body) = ChunkHeader::parse(&buf).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(body, b"hello");
    }

    #[test]
    fn header_rejects_bad_version() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = 9;
        assert!(matches!(
            ChunkHeader::parse(&buf),
            Err(VoiceError::BadVersion(9))
        ));
    }
}
