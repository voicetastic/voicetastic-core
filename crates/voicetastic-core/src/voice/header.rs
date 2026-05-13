//! 16-byte chunk header parse/serialize: 12 logical bytes + 4-byte trailing MAC.

use super::consts::{
    HEADER_MAC_LEN, HEADER_SIZE, MAX_PACKET_SIZE, MAX_PARITY_PER_MESSAGE, PROTOCOL_VERSION,
};
use super::error::{Result, VoiceError};
use super::mac::{self, MAC_KEYED_FLAG};
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
    /// Whether the trailing 4-byte MAC was computed with HMAC-SHA256 over a
    /// channel PSK (`true`) or with unkeyed SHA-256 (`false`). Set by
    /// [`Self::serialize_with_mac`] based on the caller-provided key and
    /// recovered from the on-wire flags byte by [`Self::parse`].
    pub mac_keyed: bool,
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
        if self.mac_keyed {
            b |= MAC_KEYED_FLAG;
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
    /// trailing 4-byte MAC.
    ///
    /// - `mac_key = Some(psk)` → HMAC-SHA256(psk, header[0..12])[..4];
    ///   the `mac_keyed` flag bit is set.
    /// - `mac_key = None` → SHA-256(header[0..12])[..4]; the flag bit
    ///   is cleared.
    ///
    /// `self.mac_keyed` is overwritten to match `mac_key.is_some()`, so
    /// callers don't have to keep the two in sync manually.
    pub(crate) fn serialize_with_mac(&mut self, mac_key: Option<&[u8]>) -> [u8; HEADER_SIZE] {
        self.mac_keyed = mac_key.is_some();
        let mut out = [0u8; HEADER_SIZE];
        self.write_logical(&mut out);
        let tag = mac::compute_tag(&out[..HEADER_SIZE - HEADER_MAC_LEN], mac_key);
        out[HEADER_SIZE - HEADER_MAC_LEN..].copy_from_slice(&tag);
        out
    }

    /// Parse a frame header from `bytes`, verifying the trailing MAC
    /// against `mac_key`. Returns the header and the body slice.
    ///
    /// MAC verification rules (see [`super::mac::verify`]):
    /// - Header advertises `mac_keyed = true` and `mac_key = Some` →
    ///   HMAC compare.
    /// - Header advertises `mac_keyed = true` and `mac_key = None` →
    ///   [`VoiceError::MacKeyMissing`]; receivers without the PSK
    ///   cannot validate the frame.
    /// - Header advertises `mac_keyed = false` → SHA-256 compare,
    ///   ignoring `mac_key`. Catches on-air bit-flips but offers no
    ///   authenticity.
    pub fn parse<'a>(bytes: &'a [u8], mac_key: Option<&[u8]>) -> Result<(Self, &'a [u8])> {
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
        // Bits 0x07 remain reserved (must be zero). 0x08 is the keyed-MAC
        // flag handled by [`mac::verify`]; 0x10 = last_in_stream;
        // 0x20 = encrypted; 0xC0 = packet_type.
        if tf & 0x07 != 0 {
            return Err(VoiceError::ReservedFlagSet(tf));
        }
        let packet_type = PacketType::from_bits(tf).ok_or(VoiceError::ReservedPacketType)?;
        let encrypted = tf & 0x20 != 0;
        let last_in_stream = tf & 0x10 != 0;
        let mac_keyed = tf & MAC_KEYED_FLAG != 0;
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
        // Header is structurally well-formed; verify the trailing MAC
        // before handing the body back to the caller.
        mac::verify(&bytes[..HEADER_SIZE], mac_key)?;
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
                mac_keyed,
            },
            &bytes[HEADER_SIZE..],
        ))
    }

    /// Cheap pre-MAC peek at the `message_id` field. Returns `None` if
    /// the buffer is too short or `message_id` is zero (reserved).
    ///
    /// Intended for fast-path dispatch where the caller needs to look up
    /// per-message state (e.g. the NACK listener's active-send table)
    /// *before* it knows which MAC key to verify with. The result MUST
    /// NOT be trusted for anything beyond table lookup — a full
    /// [`Self::parse`] with the correct key is still required before
    /// acting on any other header field.
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
            encrypted: true,
            last_in_stream: true,
            message_id: 0x12345678,
            codec: VoiceCodec::Opus,
            codec_param: 16,
            stream_seq: 42,
            chunk_index: 3,
            total_data: 10,
            parity_count: 4,
            mac_keyed: false,
        }
    }

    #[test]
    fn header_roundtrip_unkeyed() {
        let mut h = sample_header();
        let mut buf = vec![0u8; HEADER_SIZE + 5];
        buf[..HEADER_SIZE].copy_from_slice(&h.serialize_with_mac(None));
        buf[HEADER_SIZE..].copy_from_slice(b"hello");
        let (parsed, body) = ChunkHeader::parse(&buf, None).unwrap();
        assert!(!parsed.mac_keyed);
        assert_eq!(parsed.message_id, h.message_id);
        assert_eq!(body, b"hello");
    }

    #[test]
    fn header_roundtrip_keyed() {
        let mut h = sample_header();
        let psk = b"channel-psk";
        let mut buf = vec![0u8; HEADER_SIZE];
        buf[..HEADER_SIZE].copy_from_slice(&h.serialize_with_mac(Some(psk)));
        let (parsed, _body) = ChunkHeader::parse(&buf, Some(psk)).unwrap();
        assert!(parsed.mac_keyed);
        // Wrong key fails.
        assert!(matches!(
            ChunkHeader::parse(&buf, Some(b"wrong")),
            Err(VoiceError::BadMac),
        ));
        // No key + keyed header fails distinctively.
        assert!(matches!(
            ChunkHeader::parse(&buf, None),
            Err(VoiceError::MacKeyMissing),
        ));
    }

    #[test]
    fn header_rejects_bad_version() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = 9;
        assert!(matches!(
            ChunkHeader::parse(&buf, None),
            Err(VoiceError::BadVersion(9))
        ));
    }

    #[test]
    fn header_rejects_tampered_logical_field() {
        let mut h = sample_header();
        let mut buf = h.serialize_with_mac(None);
        // Flip a bit in `total_data` — MAC must catch it.
        buf[10] ^= 1;
        assert!(matches!(
            ChunkHeader::parse(&buf, None),
            Err(VoiceError::BadMac),
        ));
    }
}
