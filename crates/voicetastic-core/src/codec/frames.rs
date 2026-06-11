/// Per-codec frame-walking primitives shared by the encoder, decoder, and
/// duration estimator. Pure parsing; no native deps, no codec features required.
///
/// # AMR-NB IF1 frame layout
///
/// Each frame starts with a 1-byte ToC header:
/// ```text
/// bit 7      : unused (0)
/// bits 6..3  : FT (frame type, 0-15)
/// bit 2      : Q (quality flag)
/// bits 1..0  : padding
/// ```
/// The remainder (0 to 31 bytes) is the encoded payload. Total frame size
/// (ToC included) is given by [`AMRNB_FRAME_BYTES`].
///
/// # Opus length-prefix wire format
///
/// ```text
/// [u16 BE length][packet bytes] [u16 BE length][packet bytes] ...
/// ```

// ---------------------------------------------------------------------------
// AMR-NB frame size table
// ---------------------------------------------------------------------------

/// Total bytes (including the ToC byte) per AMR-NB frame for each 4-bit frame
/// type (index 0-15).
///
/// Speech modes 0-7 carry 12-31 payload bytes; mode 8 is SID (comfort noise,
/// 5-byte payload); mode 15 is NO_DATA (ToC byte only). Modes 9-14 are
/// reserved — `None` means the size is undefined and the frame cannot be
/// decoded. A DTX-enabled encoder legitimately emits SID and NO_DATA frames
/// (RFC 3267 IF1), so parsers must accept them rather than aborting.
pub const AMRNB_FRAME_BYTES: [Option<usize>; 16] = [
    Some(13), // 0: MR475   (12 payload + 1 ToC)
    Some(14), // 1: MR515
    Some(16), // 2: MR59
    Some(18), // 3: MR67
    Some(20), // 4: MR74
    Some(21), // 5: MR795
    Some(27), // 6: MR102
    Some(32), // 7: MR122
    Some(6),  // 8: SID (comfort noise, 5 payload + 1 ToC)
    None,     // 9:  reserved
    None,     // 10: reserved
    None,     // 11: reserved
    None,     // 12: reserved
    None,     // 13: reserved
    None,     // 14: reserved
    Some(1),  // 15: NO_DATA (ToC only, no payload)
];

// ---------------------------------------------------------------------------
// AmrnbFrames iterator
// ---------------------------------------------------------------------------

/// Iterator over AMR-NB IF1 frames in a payload slice.
///
/// Each item is the full frame bytes including the leading ToC byte. Iteration
/// stops at the first frame with an undefined type (reserved modes 9-14) or a
/// truncated payload. Use [`AmrnbFrames::remaining`] afterward to detect
/// leftover bytes.
pub struct AmrnbFrames<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> AmrnbFrames<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Bytes not yet consumed. Non-zero after iteration only when the stream
    /// contained a reserved frame type or a truncated payload.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
}

impl<'a> Iterator for AmrnbFrames<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data.len() {
            return None;
        }
        let toc = self.data[self.pos];
        let mode = ((toc >> 3) & 0x0F) as usize;
        let size = AMRNB_FRAME_BYTES[mode]?;
        if self.pos + size > self.data.len() {
            return None;
        }
        let frame = &self.data[self.pos..self.pos + size];
        self.pos += size;
        Some(frame)
    }
}

// ---------------------------------------------------------------------------
// OpusPackets iterator
// ---------------------------------------------------------------------------

/// Iterator over Opus packets in the length-prefixed wire format.
///
/// Each item is the raw packet bytes without the 2-byte length prefix.
/// Iteration stops at the first truncated packet; check [`OpusPackets::remaining`]
/// afterward to detect a malformed stream.
pub struct OpusPackets<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> OpusPackets<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Bytes not yet consumed. Non-zero after full iteration only when the
    /// stream ended with a truncated packet (length prefix present but payload
    /// shorter than declared).
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
}

impl<'a> Iterator for OpusPackets<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos + 2 > self.data.len() {
            return None;
        }
        let len = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]) as usize;
        if self.pos + 2 + len > self.data.len() {
            // Don't advance pos: the truncated packet stays as "remaining" bytes.
            return None;
        }
        self.pos += 2;
        let pkt = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Some(pkt)
    }
}

// ---------------------------------------------------------------------------
// opus_packet_samples_48k
// ---------------------------------------------------------------------------

/// Total PCM samples at 48 kHz contained in an Opus packet, from the TOC byte.
///
/// Handles all 32 configurations defined in RFC 6716 Section 3.1, and all four
/// frame codes (0 = 1 frame, 1 = 2 CBR frames, 2 = 2 VBR frames, 3 = N frames
/// with N given by the count byte). Returns `None` only if the packet is empty
/// or if code 3 is used but the required count byte is missing.
///
/// To convert to duration in milliseconds: `samples / 48`.
pub fn opus_packet_samples_48k(pkt: &[u8]) -> Option<u32> {
    let toc = *pkt.first()?;
    let config = (toc >> 3) as usize; // 0..=31
    let code = toc & 0x03;

    // Frame duration in samples at 48 kHz for this config (RFC 6716 §3.1 table).
    let frame_samples: u32 = match config {
        // SILK narrowband / mediumband / wideband: 10 / 20 / 40 / 60 ms
        0 | 4 | 8 => 480,
        1 | 5 | 9 => 960,
        2 | 6 | 10 => 1920,
        3 | 7 | 11 => 2880,
        // Hybrid (SWB / FB): 10 / 20 ms
        12 | 14 => 480,
        13 | 15 => 960,
        // CELT (NB / WB / SWB / FB): 2.5 / 5 / 10 / 20 ms
        16 | 20 | 24 | 28 => 120,
        17 | 21 | 25 | 29 => 240,
        18 | 22 | 26 | 30 => 480,
        19 | 23 | 27 | 31 => 960,
        // config is (toc >> 3) so max value is 31; this arm is unreachable.
        _ => return None,
    };

    // Frames per packet from frame code.
    let frames: u32 = match code {
        0 => 1,
        1 | 2 => 2,
        3 => (pkt.get(1)? & 0x3F) as u32, // count byte, low 6 bits
        _ => return None,
    };

    Some(frame_samples * frames)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- AMRNB_FRAME_BYTES ---

    #[test]
    fn amrnb_frame_bytes_speech_modes() {
        let expected = [13usize, 14, 16, 18, 20, 21, 27, 32];
        for (mode, &exp) in expected.iter().enumerate() {
            assert_eq!(AMRNB_FRAME_BYTES[mode], Some(exp), "mode {mode}");
        }
    }

    #[test]
    fn amrnb_frame_bytes_dtx_frames() {
        assert_eq!(AMRNB_FRAME_BYTES[8], Some(6), "SID");
        assert_eq!(AMRNB_FRAME_BYTES[15], Some(1), "NO_DATA");
    }

    #[test]
    fn amrnb_frame_bytes_reserved_are_none() {
        for mode in 9..=14 {
            assert!(AMRNB_FRAME_BYTES[mode].is_none(), "mode {mode} should be None");
        }
    }

    // --- AmrnbFrames ---

    fn amrnb_toc(mode: u8) -> u8 {
        // ToC: bits 6..3 = FT, other bits 0
        (mode & 0x0F) << 3
    }

    #[test]
    fn amrnb_frames_speech() {
        // One MR475 frame (mode 0, total 13 bytes)
        let mut payload = vec![amrnb_toc(0)];
        payload.extend_from_slice(&[0xAB; 12]);
        let frames: Vec<&[u8]> = AmrnbFrames::new(&payload).collect();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 13);
        assert_eq!(frames[0][0], amrnb_toc(0));
    }

    #[test]
    fn amrnb_frames_counts_sid_and_no_data() {
        // MR475 (13) + SID (6) + NO_DATA (1)
        let mut payload = vec![amrnb_toc(0)];
        payload.extend_from_slice(&[0u8; 12]);
        payload.push(amrnb_toc(8));
        payload.extend_from_slice(&[0u8; 5]);
        payload.push(amrnb_toc(15));

        let mut iter = AmrnbFrames::new(&payload);
        let frames: Vec<&[u8]> = iter.by_ref().collect();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].len(), 13);
        assert_eq!(frames[1].len(), 6);
        assert_eq!(frames[2].len(), 1);
        assert_eq!(iter.remaining(), 0);
    }

    #[test]
    fn amrnb_frames_stops_on_reserved_mode() {
        // Mode 9 is reserved
        let payload = [amrnb_toc(9)];
        let mut iter = AmrnbFrames::new(&payload);
        assert!(iter.next().is_none());
        assert_eq!(iter.remaining(), 1);
    }

    #[test]
    fn amrnb_frames_stops_on_truncated_payload() {
        // MR475 promises 13 bytes but only 5 are present
        let payload = [amrnb_toc(0), 0, 0, 0, 0];
        let mut iter = AmrnbFrames::new(&payload);
        assert!(iter.next().is_none());
        assert_eq!(iter.remaining(), 5);
    }

    // --- OpusPackets ---

    fn make_opus_payload(lengths: &[u16]) -> Vec<u8> {
        let mut out = Vec::new();
        for &len in lengths {
            out.extend_from_slice(&len.to_be_bytes());
            out.extend(std::iter::repeat(0xCC).take(len as usize));
        }
        out
    }

    #[test]
    fn opus_packets_empty_payload() {
        let pkts: Vec<&[u8]> = OpusPackets::new(&[]).collect();
        assert_eq!(pkts.len(), 0);
    }

    #[test]
    fn opus_packets_multiple() {
        let payload = make_opus_payload(&[10, 20, 5]);
        let pkts: Vec<&[u8]> = OpusPackets::new(&payload).collect();
        assert_eq!(pkts.len(), 3);
        assert_eq!(pkts[0].len(), 10);
        assert_eq!(pkts[1].len(), 20);
        assert_eq!(pkts[2].len(), 5);
    }

    #[test]
    fn opus_packets_truncated_leaves_remaining() {
        // Length prefix says 20 bytes but only 3 bytes follow
        let mut payload = 20u16.to_be_bytes().to_vec();
        payload.extend_from_slice(&[0u8; 3]);
        let mut iter = OpusPackets::new(&payload);
        assert!(iter.next().is_none());
        // pos was NOT advanced past the length prefix on truncation
        assert_eq!(iter.remaining(), 5);
    }

    #[test]
    fn opus_packets_clean_stream_has_no_remaining() {
        let payload = make_opus_payload(&[15, 30]);
        let mut iter = OpusPackets::new(&payload);
        while iter.next().is_some() {}
        assert_eq!(iter.remaining(), 0);
    }

    // --- opus_packet_samples_48k ---

    #[test]
    fn opus_samples_silk_wb_20ms() {
        // Config 9 = SILK WB 20ms (TOC = 9 << 3 = 0x48), code 0 = 1 frame
        let toc = 0x48u8; // config=9, code=0
        let pkt = [toc, 0xAB, 0xCD];
        assert_eq!(opus_packet_samples_48k(&pkt), Some(960));
    }

    #[test]
    fn opus_samples_celt_2_5ms() {
        // Config 16 = CELT NB 2.5ms (TOC = 16 << 3 = 0x80), code 0 = 1 frame
        let toc = 0x80u8;
        assert_eq!(opus_packet_samples_48k(&[toc, 0]), Some(120));
    }

    #[test]
    fn opus_samples_code1_doubles_frames() {
        // Config 9, code 1: 2 CBR frames of 20ms each = 1920 samples
        let toc = (9u8 << 3) | 1;
        assert_eq!(opus_packet_samples_48k(&[toc, 0, 0]), Some(1920));
    }

    #[test]
    fn opus_samples_code3_uses_count_byte() {
        // Config 9, code 3: count byte = 0x03 (low 6 bits) => 3 frames of 20ms
        let toc = (9u8 << 3) | 3;
        let count = 3u8;
        assert_eq!(opus_packet_samples_48k(&[toc, count, 0, 0]), Some(2880));
    }

    #[test]
    fn opus_samples_empty_returns_none() {
        assert_eq!(opus_packet_samples_48k(&[]), None);
    }

    #[test]
    fn opus_samples_60ms_silk() {
        // Config 3 = SILK NB 60ms
        let toc = 3u8 << 3;
        assert_eq!(opus_packet_samples_48k(&[toc, 0]), Some(2880));
    }
}
