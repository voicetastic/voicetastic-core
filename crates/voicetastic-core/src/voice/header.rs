//! 16-byte chunk header parse/serialize: 12 logical bytes + 4-byte trailing MAC.

use super::consts::{
    HEADER_MAC_LEN, HEADER_SIZE, MAX_PACKET_SIZE, MAX_PARITY_PER_MESSAGE, PROTOCOL_VERSION,
};
use super::error::{Result, VoiceError};
use super::mac;
use super::types::{PacketType, VoiceCodec};

/// Parsed view of a chunk header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHeader {
    pub packet_type: PacketType,
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
        if self.last_in_stream {
            b |= 0x10;
        }
        b
    }

    fn write_logical(&self, out: &mut [u8; HEADER_SIZE]) {
        out[0] = PROTOCOL_VERSION;
        out[1] = self.type_flags_byte();
        out[2..6].copy_from_slice(&self.message_id.to_be_bytes());
        out[6] = self.codec.to_byte();
        out[7] = self.codec_param;
        out[8] = self.stream_seq;
        out[9] = self.chunk_index;
        out[10] = self.total_data;
        out[11] = self.parity_count;
    }

    /// Serialise to a fixed [`HEADER_SIZE`]-byte buffer, computing the
    /// trailing 4-byte SHA-256 integrity tag.
    pub(crate) fn serialize(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        self.write_logical(&mut out);
        let tag = mac::compute_tag(&out[..HEADER_SIZE - HEADER_MAC_LEN]);
        out[HEADER_SIZE - HEADER_MAC_LEN..].copy_from_slice(&tag);
        out
    }

    /// Parse a frame header from `bytes`, verifying the trailing MAC.
    /// Returns the header and the body slice.
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
        // Bits 0x2F are reserved (must be zero): 0x07 unused, 0x08 was the
        // V2 keyed-MAC flag, 0x20 was the V2 encrypted flag. A V3 parser
        // rejects any of them being set so a V2 frame that survives the
        // version check (it shouldn't, but defense in depth) fails fast
        // rather than silently producing garbage.
        if tf & 0x2F != 0 {
            return Err(VoiceError::ReservedFlagSet(tf));
        }
        let packet_type = PacketType::from_bits(tf).ok_or(VoiceError::ReservedPacketType)?;
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
                // Spec §3.4: NACK frames carry chunk_index = 0.
                if chunk_index != 0 {
                    return Err(VoiceError::BadNackIndex(chunk_index));
                }
            }
        }
        // Header is structurally well-formed; verify the trailing MAC
        // before handing the body back to the caller.
        mac::verify(&bytes[..HEADER_SIZE])?;
        Ok((
            Self {
                packet_type,
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

    /// Cheap pre-MAC peek at the `message_id` field. Returns `None` if
    /// the buffer is too short or `message_id` is zero (reserved).
    ///
    /// Intended for fast-path dispatch where the caller needs to look up
    /// per-message state (e.g. the NACK listener's active-send table)
    /// *before* it knows whether the frame is for them. The result MUST
    /// NOT be trusted for anything beyond table lookup — a full
    /// [`Self::parse`] is still required before acting on any other
    /// header field.
    pub fn peek_message_id(bytes: &[u8]) -> Option<u32> {
        if bytes.len() < HEADER_SIZE || bytes[0] != PROTOCOL_VERSION {
            return None;
        }
        let id = u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
        (id != 0).then_some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::consts::HEADER_SIZE;
    use super::*;

    fn sample_header() -> ChunkHeader {
        ChunkHeader {
            packet_type: PacketType::Data,
            last_in_stream: true,
            message_id: 0x12345678,
            codec: VoiceCodec::Opus,
            codec_param: 16,
            stream_seq: 42,
            chunk_index: 3,
            total_data: 10,
            parity_count: 4,
        }
    }

    #[test]
    fn header_roundtrip() {
        let h = sample_header();
        let mut buf = vec![0u8; HEADER_SIZE + 5];
        buf[..HEADER_SIZE].copy_from_slice(&h.serialize());
        buf[HEADER_SIZE..].copy_from_slice(b"hello");
        let (parsed, body) = ChunkHeader::parse(&buf).unwrap();
        assert_eq!(parsed.message_id, h.message_id);
        assert_eq!(body, b"hello");
    }

    #[test]
    fn header_rejects_bad_version() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = 0x02; // old V2
        assert!(matches!(
            ChunkHeader::parse(&buf),
            Err(VoiceError::BadVersion(0x02))
        ));
    }

    #[test]
    fn header_rejects_v2_encrypted_bit() {
        // Build a syntactically-valid V3 header, then set the legacy
        // encrypted flag (0x20). Parser must reject as ReservedFlagSet
        // before any MAC verification, so a V2 frame that survives the
        // version check still fails fast.
        let h = sample_header();
        let mut buf = h.serialize();
        buf[1] |= 0x20;
        assert!(matches!(
            ChunkHeader::parse(&buf),
            Err(VoiceError::ReservedFlagSet(_))
        ));
    }

    #[test]
    fn header_rejects_v2_keyed_mac_bit() {
        let h = sample_header();
        let mut buf = h.serialize();
        buf[1] |= 0x08;
        assert!(matches!(
            ChunkHeader::parse(&buf),
            Err(VoiceError::ReservedFlagSet(_))
        ));
    }

    #[test]
    fn header_rejects_tampered_logical_field() {
        let h = sample_header();
        let mut buf = h.serialize();
        // Flip a bit in `total_data` — MAC must catch it.
        buf[10] ^= 1;
        assert!(matches!(ChunkHeader::parse(&buf), Err(VoiceError::BadMac)));
    }
}
