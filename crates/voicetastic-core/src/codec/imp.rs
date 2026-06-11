use std::ffi::c_void;

use codec2::{Codec2, Codec2Mode};
use opus::{Application, Bandwidth, Bitrate, Channels, Decoder, Encoder as OpusEncoder};

use super::error::CodecError;
use super::resampler::Resampler;
use super::{AMRNB_SAMPLE_RATE_HZ, AMRNB_SAMPLES_PER_FRAME, CODEC2_SAMPLE_RATE_HZ, OPUS_BITRATE};
use super::{FRAME_SAMPLES, OpusBandwidth, SAMPLE_RATE_HZ};
use crate::voice::VoiceCodec;

// ---------------------------------------------------------------------------
// AMR-NB raw FFI (no -sys crate exists on crates.io for libopencore-amrnb)
// ---------------------------------------------------------------------------

#[link(name = "opencore-amrnb")]
unsafe extern "C" {
    fn Encoder_Interface_init(dtx: i32) -> *mut c_void;
    fn Encoder_Interface_Encode(
        st: *mut c_void,
        mode: i32,
        speech: *const i16,
        serial: *mut u8,
        force_speech: i32,
    ) -> i32;
    fn Encoder_Interface_exit(st: *mut c_void);

    fn Decoder_Interface_init() -> *mut c_void;
    fn Decoder_Interface_Decode(st: *mut c_void, input: *const u8, out: *mut i16, bfi: i32);
    fn Decoder_Interface_exit(st: *mut c_void);
}

struct AmrnbEncoder(*mut c_void);
impl AmrnbEncoder {
    fn new() -> Result<Self, CodecError> {
        let p = unsafe { Encoder_Interface_init(0) };
        if p.is_null() {
            return Err(CodecError::Codec(
                "amrnb: Encoder_Interface_init failed".into(),
            ));
        }
        Ok(Self(p))
    }
}
impl Drop for AmrnbEncoder {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { Encoder_Interface_exit(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

struct AmrnbDecoder(*mut c_void);
impl AmrnbDecoder {
    fn new() -> Result<Self, CodecError> {
        let p = unsafe { Decoder_Interface_init() };
        if p.is_null() {
            return Err(CodecError::Codec(
                "amrnb: Decoder_Interface_init failed".into(),
            ));
        }
        Ok(Self(p))
    }
}
impl Drop for AmrnbDecoder {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { Decoder_Interface_exit(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

// ---------------------------------------------------------------------------
// Codec2 mode mapper
// ---------------------------------------------------------------------------

fn codec2_mode_from_byte(b: u8) -> Result<Codec2Mode, CodecError> {
    Ok(match b {
        0 => Codec2Mode::MODE_3200,
        1 => Codec2Mode::MODE_2400,
        2 => Codec2Mode::MODE_1600,
        3 => Codec2Mode::MODE_1400,
        4 => Codec2Mode::MODE_1300,
        5 => Codec2Mode::MODE_1200,
        _ => {
            return Err(CodecError::Codec(format!("unknown codec2 mode index {b}")));
        }
    })
}

// ---------------------------------------------------------------------------
// AMR-NB helpers
// ---------------------------------------------------------------------------

fn amrnb_validate_mode(b: u8) -> Result<i32, CodecError> {
    if b < 8 {
        Ok(b as i32)
    } else {
        Err(CodecError::Codec(format!("unknown amrnb mode index {b}")))
    }
}

// ---------------------------------------------------------------------------
// Streaming encoder state
// ---------------------------------------------------------------------------

enum EncState {
    Opus {
        encoder: OpusEncoder,
        scratch: Vec<f32>,
        payload: Vec<u8>,
    },
    Codec2 {
        c2: Box<Codec2>,
        samples_per_frame: usize,
        bytes_per_frame: usize,
        resampler: Resampler,
        scratch: Vec<f32>,
        payload: Vec<u8>,
    },
    AmrNb {
        enc: AmrnbEncoder,
        mode: i32,
        bytes_per_frame: usize,
        resampler: Resampler,
        scratch: Vec<f32>,
        payload: Vec<u8>,
    },
}

impl EncState {
    fn new(
        codec_id: VoiceCodec,
        codec_param: u8,
        opus_bw: OpusBandwidth,
    ) -> Result<Self, CodecError> {
        match codec_id {
            VoiceCodec::Opus => {
                let mut enc = OpusEncoder::new(SAMPLE_RATE_HZ, Channels::Mono, Application::Voip)
                    .map_err(|e| CodecError::Codec(e.to_string()))?;
                let bps = if codec_param == 0 {
                    OPUS_BITRATE
                } else {
                    i32::from(codec_param) * 1000
                };
                enc.set_bitrate(Bitrate::Bits(bps))
                    .map_err(|e| CodecError::Codec(e.to_string()))?;
                let bw = match opus_bw {
                    OpusBandwidth::Narrow => Bandwidth::Narrowband,
                    OpusBandwidth::Wide => Bandwidth::Wideband,
                };
                enc.set_bandwidth(bw)
                    .map_err(|e| CodecError::Codec(e.to_string()))?;
                Ok(Self::Opus {
                    encoder: enc,
                    scratch: Vec::with_capacity(FRAME_SAMPLES),
                    payload: Vec::new(),
                })
            }
            VoiceCodec::Codec2 => {
                let mode = codec2_mode_from_byte(codec_param)?;
                let c2 = Codec2::new(mode);
                let samples_per_frame = c2.samples_per_frame();
                let bytes_per_frame = c2.bits_per_frame().div_ceil(8);
                Ok(Self::Codec2 {
                    c2: Box::new(c2),
                    samples_per_frame,
                    bytes_per_frame,
                    resampler: Resampler::new(SAMPLE_RATE_HZ, CODEC2_SAMPLE_RATE_HZ),
                    scratch: Vec::with_capacity(samples_per_frame * 2),
                    payload: Vec::new(),
                })
            }
            VoiceCodec::AmrNb => {
                let mode = amrnb_validate_mode(codec_param)?;
                let enc = AmrnbEncoder::new()?;
                // mode is 0..7 (validated); all speech modes are Some(_).
                let bytes_per_frame = super::frames::AMRNB_FRAME_BYTES[mode as usize]
                    .expect("speech mode always has a defined frame size");
                Ok(Self::AmrNb {
                    enc,
                    mode,
                    bytes_per_frame,
                    resampler: Resampler::new(SAMPLE_RATE_HZ, AMRNB_SAMPLE_RATE_HZ),
                    scratch: Vec::with_capacity(AMRNB_SAMPLES_PER_FRAME * 2),
                    payload: Vec::new(),
                })
            }
            other => Err(CodecError::UnsupportedCodec(other)),
        }
    }

    fn push(&mut self, src: &[f32]) -> Result<(), CodecError> {
        match self {
            Self::Opus {
                encoder,
                scratch,
                payload,
            } => {
                scratch.extend_from_slice(src);
                let mut buf = [0u8; 1275];
                while scratch.len() >= FRAME_SAMPLES {
                    let mut frame = [0i16; FRAME_SAMPLES];
                    for (i, s) in scratch.drain(..FRAME_SAMPLES).enumerate() {
                        frame[i] = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                    let n = encoder
                        .encode(&frame, &mut buf)
                        .map_err(|e| CodecError::Codec(e.to_string()))?;
                    payload.extend_from_slice(&(n as u16).to_be_bytes());
                    payload.extend_from_slice(&buf[..n]);
                }
                Ok(())
            }
            Self::Codec2 {
                c2,
                samples_per_frame,
                bytes_per_frame,
                resampler,
                scratch,
                payload,
            } => {
                resampler.push(src, scratch);
                while scratch.len() >= *samples_per_frame {
                    let mut frame_i16 = vec![0i16; *samples_per_frame];
                    for (i, s) in scratch.drain(..*samples_per_frame).enumerate() {
                        frame_i16[i] = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                    let mut packed = vec![0u8; *bytes_per_frame];
                    c2.encode(&mut packed, &frame_i16);
                    payload.extend_from_slice(&packed);
                }
                Ok(())
            }
            Self::AmrNb {
                enc,
                mode,
                bytes_per_frame,
                resampler,
                scratch,
                payload,
            } => {
                resampler.push(src, scratch);
                let mut serial = [0u8; 64];
                while scratch.len() >= AMRNB_SAMPLES_PER_FRAME {
                    let mut frame_i16 = [0i16; AMRNB_SAMPLES_PER_FRAME];
                    for (i, s) in scratch.drain(..AMRNB_SAMPLES_PER_FRAME).enumerate() {
                        frame_i16[i] = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                    let n = unsafe {
                        Encoder_Interface_Encode(
                            enc.0,
                            *mode,
                            frame_i16.as_ptr(),
                            serial.as_mut_ptr(),
                            0,
                        )
                    };
                    if n <= 0 {
                        return Err(CodecError::Codec(format!(
                            "amrnb: Encoder_Interface_Encode returned {n}"
                        )));
                    }
                    let n = n as usize;
                    if n != *bytes_per_frame {
                        return Err(CodecError::Codec(format!(
                            "amrnb: unexpected frame size {n}, want {bytes_per_frame}"
                        )));
                    }
                    payload.extend_from_slice(&serial[..n]);
                }
                Ok(())
            }
        }
    }

    fn finish(mut self) -> Result<Vec<u8>, CodecError> {
        match &mut self {
            Self::Opus {
                encoder,
                scratch,
                payload,
            } => {
                if !scratch.is_empty() {
                    scratch.resize(FRAME_SAMPLES, 0.0);
                    let mut frame = [0i16; FRAME_SAMPLES];
                    for (i, s) in scratch.drain(..FRAME_SAMPLES).enumerate() {
                        frame[i] = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                    let mut buf = [0u8; 1275];
                    let n = encoder
                        .encode(&frame, &mut buf)
                        .map_err(|e| CodecError::Codec(e.to_string()))?;
                    payload.extend_from_slice(&(n as u16).to_be_bytes());
                    payload.extend_from_slice(&buf[..n]);
                }
            }
            Self::Codec2 {
                c2,
                samples_per_frame,
                bytes_per_frame,
                scratch,
                payload,
                ..
            } => {
                if !scratch.is_empty() {
                    scratch.resize(*samples_per_frame, 0.0);
                    let mut frame_i16 = vec![0i16; *samples_per_frame];
                    for (i, s) in scratch.drain(..*samples_per_frame).enumerate() {
                        frame_i16[i] = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                    let mut packed = vec![0u8; *bytes_per_frame];
                    c2.encode(&mut packed, &frame_i16);
                    payload.extend_from_slice(&packed);
                }
            }
            Self::AmrNb {
                enc,
                mode,
                bytes_per_frame,
                scratch,
                payload,
                ..
            } => {
                if !scratch.is_empty() {
                    scratch.resize(AMRNB_SAMPLES_PER_FRAME, 0.0);
                    let mut frame_i16 = [0i16; AMRNB_SAMPLES_PER_FRAME];
                    for (i, s) in scratch.drain(..AMRNB_SAMPLES_PER_FRAME).enumerate() {
                        frame_i16[i] = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                    let mut serial = [0u8; 64];
                    let n = unsafe {
                        Encoder_Interface_Encode(
                            enc.0,
                            *mode,
                            frame_i16.as_ptr(),
                            serial.as_mut_ptr(),
                            0,
                        )
                    };
                    if n > 0 {
                        let n = n as usize;
                        if n == *bytes_per_frame {
                            payload.extend_from_slice(&serial[..n]);
                        } else {
                            tracing::warn!(
                                n,
                                expected = *bytes_per_frame,
                                "AMR-NB final frame size mismatch; dropping frame",
                            );
                        }
                    }
                }
            }
        }
        Ok(match self {
            Self::Opus { payload, .. }
            | Self::Codec2 { payload, .. }
            | Self::AmrNb { payload, .. } => payload,
        })
    }
}

// ---------------------------------------------------------------------------
// Public API: Encoder
// ---------------------------------------------------------------------------

pub struct Encoder(EncState);

impl Encoder {
    pub fn new(
        codec_id: VoiceCodec,
        codec_param: u8,
        opus_bw: OpusBandwidth,
    ) -> Result<Self, CodecError> {
        Ok(Self(EncState::new(codec_id, codec_param, opus_bw)?))
    }

    pub fn push(&mut self, src: &[f32]) -> Result<(), CodecError> {
        self.0.push(src)
    }

    pub fn finish(self) -> Result<Vec<u8>, CodecError> {
        self.0.finish()
    }
}

// ---------------------------------------------------------------------------
// Public API: decode
// ---------------------------------------------------------------------------

pub fn decode(
    payload: &[u8],
    codec_id: VoiceCodec,
    codec_param: u8,
) -> Result<Vec<i16>, CodecError> {
    decode_with_gaps(payload, &[], codec_id, codec_param)
}

/// Decode a (possibly partial) payload, concealing the byte ranges in `gaps`
/// (zero-padded missing chunks) instead of decoding the padding as audio.
///
/// `gaps` is the [`crate::voice::VoiceMessage::gaps`] list. When it is empty
/// this is identical to [`decode`].
///
/// Concealment is per-codec:
/// - **Codec2** emits true silence for frames overlapping a gap (fixed frame
///   size ⇒ frame boundaries are deterministic regardless of the zero bytes).
/// - **AMR-NB** feeds one NO_DATA frame per lost frame, so the decoder runs
///   its packet-loss concealment and playback timing is preserved. Frame size
///   comes from `codec_param` (the fixed encoder mode), not the zero bytes.
/// - **Opus** is **deprecated** for this protocol (variable-rate, length-
///   prefixed packets are too heavy and cannot be concealed by this fixed-
///   frame scheme); `gaps` is ignored and the present packets are decoded
///   best-effort. Callers should gate Opus partials off (`gaps.is_empty()`).
pub fn decode_with_gaps(
    payload: &[u8],
    gaps: &[std::ops::Range<usize>],
    codec_id: VoiceCodec,
    codec_param: u8,
) -> Result<Vec<i16>, CodecError> {
    match codec_id {
        // Opus: deprecated; no fixed-frame concealment. Decode what arrived.
        VoiceCodec::Opus => decode_opus(payload),
        VoiceCodec::Codec2 => decode_codec2(payload, codec_param, gaps),
        VoiceCodec::AmrNb => decode_amrnb(payload, codec_param, gaps),
        other => Err(CodecError::UnsupportedCodec(other)),
    }
}

/// True if the `[start, start + len)` frame overlaps any gap range. A frame
/// straddling a present/gap boundary counts as a gap (concealed), since part
/// of its bytes are zero padding.
fn frame_overlaps_gap(start: usize, len: usize, gaps: &[std::ops::Range<usize>]) -> bool {
    let end = start + len;
    gaps.iter().any(|g| start < g.end && g.start < end)
}

fn decode_opus(payload: &[u8]) -> Result<Vec<i16>, CodecError> {
    // Opus packets are self-describing and may legally carry up to 120 ms
    // (5760 samples @ 48 kHz mono), not just the 20 ms (FRAME_SAMPLES) we
    // encode. Size the output buffer to that legal maximum so a peer using
    // larger frames doesn't make libopus return OPUS_BUFFER_TOO_SMALL and
    // abort the whole clip. Matches the pure-Rust decoder path in opus_d.rs.
    const OPUS_MAX_FRAME_SAMPLES: usize = 5760;
    let mut dec = Decoder::new(SAMPLE_RATE_HZ, Channels::Mono)
        .map_err(|e| CodecError::Codec(e.to_string()))?;
    let mut pcm: Vec<i16> = Vec::new();
    let mut frame = [0i16; OPUS_MAX_FRAME_SAMPLES];
    let mut packets = super::frames::OpusPackets::new(payload);
    // `by_ref()` so `packets` stays usable for the truncation check below.
    for pkt in packets.by_ref() {
        let n = dec
            .decode(pkt, &mut frame[..], false)
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        pcm.extend_from_slice(&frame[..n]);
    }
    if packets.remaining() > 0 {
        return Err(CodecError::Codec("truncated opus stream".into()));
    }
    Ok(pcm)
}

fn decode_codec2(
    payload: &[u8],
    codec_param: u8,
    gaps: &[std::ops::Range<usize>],
) -> Result<Vec<i16>, CodecError> {
    let mode = codec2_mode_from_byte(codec_param)?;
    let mut c2 = Codec2::new(mode);
    let samples_per_frame = c2.samples_per_frame();
    let bytes_per_frame = c2.bits_per_frame().div_ceil(8);
    if bytes_per_frame == 0 {
        return Err(CodecError::Codec("codec2: zero-size frame".into()));
    }
    let mut pcm8k_i16: Vec<i16> =
        Vec::with_capacity((payload.len() / bytes_per_frame) * samples_per_frame);
    let mut frame = vec![0i16; samples_per_frame];
    let mut i = 0;
    while i + bytes_per_frame <= payload.len() {
        if frame_overlaps_gap(i, bytes_per_frame, gaps) {
            // Missing chunk → true silence, one frame's worth, to keep timing.
            pcm8k_i16.resize(pcm8k_i16.len() + samples_per_frame, 0);
        } else {
            c2.decode(&mut frame, &payload[i..i + bytes_per_frame]);
            pcm8k_i16.extend_from_slice(&frame);
        }
        i += bytes_per_frame;
    }
    let pcm8k_f32: Vec<f32> = pcm8k_i16
        .into_iter()
        .map(|s| s as f32 / i16::MAX as f32)
        .collect();
    let mut rs = Resampler::new(CODEC2_SAMPLE_RATE_HZ, SAMPLE_RATE_HZ);
    let mut pcm48k_f32: Vec<f32> = Vec::with_capacity(pcm8k_f32.len() * 6);
    rs.push(&pcm8k_f32, &mut pcm48k_f32);
    Ok(pcm48k_f32
        .into_iter()
        .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect())
}

/// NO_DATA octet (frame type 15, Q=1): one ToC byte, no payload. Fed to the
/// decoder for each lost frame so it runs PLC and emits a 20 ms block.
const AMRNB_NO_DATA_ARR: [u8; 1] = [0x7C];

fn decode_amrnb(
    payload: &[u8],
    codec_param: u8,
    gaps: &[std::ops::Range<usize>],
) -> Result<Vec<i16>, CodecError> {
    let dec = AmrnbDecoder::new()?;
    let mut pcm8k_i16: Vec<i16> = Vec::new();
    let mut out_frame = [0i16; AMRNB_SAMPLES_PER_FRAME];

    if gaps.is_empty() {
        // Complete payload: walk the self-describing IF1 stream as before.
        let mut iter = super::frames::AmrnbFrames::new(payload);
        // `by_ref()` so `iter` stays usable for the truncation check below.
        for frame_bytes in iter.by_ref() {
            // The decoder reads the frame type from the ToC and produces one
            // 160-sample block for every type, including comfort noise for SID
            // and PLC/silence for NO_DATA, so feeding the frame as-is preserves
            // playback timing across DTX gaps.
            unsafe {
                Decoder_Interface_Decode(dec.0, frame_bytes.as_ptr(), out_frame.as_mut_ptr(), 0);
            }
            pcm8k_i16.extend_from_slice(&out_frame);
        }
        if iter.remaining() > 0 {
            let bad_pos = payload.len() - iter.remaining();
            let toc = payload[bad_pos];
            let mode = ((toc >> 3) & 0x0F) as usize;
            if super::frames::AMRNB_FRAME_BYTES[mode].is_none() {
                return Err(CodecError::Codec(format!(
                    "amrnb: unsupported ToC byte {toc:#x}"
                )));
            }
            return Err(CodecError::Codec("amrnb: truncated frame".into()));
        }
    } else {
        // Partial payload: gap bytes are zeros and would be miswalked as
        // mode-0 frames, losing frame sync past the first gap. Because the
        // encoder runs a single fixed mode (DTX off), every frame is the same
        // size, so frame boundaries are deterministic at multiples of
        // `frame_bytes`. Step in fixed strides; feed real frames through and a
        // NO_DATA frame for any frame overlapping a gap.
        let mode = amrnb_validate_mode(codec_param)?;
        let frame_bytes = super::frames::AMRNB_FRAME_BYTES[mode as usize]
            .expect("validated speech mode always has a defined frame size");
        let mut i = 0;
        while i + frame_bytes <= payload.len() {
            let ptr = if frame_overlaps_gap(i, frame_bytes, gaps) {
                AMRNB_NO_DATA_ARR.as_ptr()
            } else {
                payload[i..].as_ptr()
            };
            unsafe {
                Decoder_Interface_Decode(dec.0, ptr, out_frame.as_mut_ptr(), 0);
            }
            pcm8k_i16.extend_from_slice(&out_frame);
            i += frame_bytes;
        }
    }

    let pcm8k_f32: Vec<f32> = pcm8k_i16
        .into_iter()
        .map(|s| s as f32 / i16::MAX as f32)
        .collect();
    let mut rs = Resampler::new(AMRNB_SAMPLE_RATE_HZ, SAMPLE_RATE_HZ);
    let mut pcm48k_f32: Vec<f32> = Vec::with_capacity(pcm8k_f32.len() * 6);
    rs.push(&pcm8k_f32, &mut pcm48k_f32);
    Ok(pcm48k_f32
        .into_iter()
        .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect())
}
