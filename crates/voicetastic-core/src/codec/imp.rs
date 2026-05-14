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

const AMRNB_BYTES_PER_FRAME: [usize; 8] = [13, 14, 16, 18, 20, 21, 27, 32];

fn amrnb_validate_mode(b: u8) -> Result<i32, CodecError> {
    if (b as usize) < AMRNB_BYTES_PER_FRAME.len() {
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
                let bytes_per_frame = AMRNB_BYTES_PER_FRAME[mode as usize];
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
    match codec_id {
        VoiceCodec::Opus => decode_opus(payload),
        VoiceCodec::Codec2 => decode_codec2(payload, codec_param),
        VoiceCodec::AmrNb => decode_amrnb(payload),
        other => Err(CodecError::UnsupportedCodec(other)),
    }
}

fn decode_opus(payload: &[u8]) -> Result<Vec<i16>, CodecError> {
    let mut dec = Decoder::new(SAMPLE_RATE_HZ, Channels::Mono)
        .map_err(|e| CodecError::Codec(e.to_string()))?;
    let mut pcm: Vec<i16> = Vec::new();
    let mut i = 0;
    let mut frame = [0i16; FRAME_SAMPLES];
    while i + 2 <= payload.len() {
        let len = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
        i += 2;
        if i + len > payload.len() {
            return Err(CodecError::Codec("truncated opus stream".into()));
        }
        let pkt = &payload[i..i + len];
        i += len;
        let n = dec
            .decode(pkt, &mut frame[..], false)
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        pcm.extend_from_slice(&frame[..n]);
    }
    Ok(pcm)
}

fn decode_codec2(payload: &[u8], codec_param: u8) -> Result<Vec<i16>, CodecError> {
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
        c2.decode(&mut frame, &payload[i..i + bytes_per_frame]);
        pcm8k_i16.extend_from_slice(&frame);
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

fn decode_amrnb(payload: &[u8]) -> Result<Vec<i16>, CodecError> {
    let dec = AmrnbDecoder::new()?;
    let mut pcm8k_i16: Vec<i16> = Vec::new();
    let mut i = 0;
    let mut frame = [0i16; AMRNB_SAMPLES_PER_FRAME];
    while i < payload.len() {
        let toc = payload[i];
        let mode = ((toc >> 3) & 0x0F) as usize;
        let Some(&size) = AMRNB_BYTES_PER_FRAME.get(mode) else {
            return Err(CodecError::Codec(format!(
                "amrnb: unsupported ToC byte {toc:#x}"
            )));
        };
        if i + size > payload.len() {
            return Err(CodecError::Codec("amrnb: truncated frame".into()));
        }
        unsafe {
            Decoder_Interface_Decode(dec.0, payload[i..].as_ptr(), frame.as_mut_ptr(), 0);
        }
        pcm8k_i16.extend_from_slice(&frame);
        i += size;
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
