//! Microphone capture, Opus / Codec2 / AMR-NB encode/decode and speaker playback for
//! voice messages composed in the Chat tab.
//!
//! Gated behind the `audio` Cargo feature so default builds remain free of
//! `cpal` (ALSA on Linux), `audiopus` (libopus), the `codec2` crate and the
//! `libopencore-amrnb` system library.
//! With the feature off the module exposes the same surface but every
//! entry point returns [`AudioError::FeatureDisabled`] and
//! [`is_available`] is `false`.
//!
//! # Wire formats
//!
//! Per-codec serialisation of [`RecordedClip::payload`]:
//!
//! - **Opus** (`VoiceCodec::Opus`, `codec_param = 0`): a sequence of
//!   length-prefixed packets:
//!
//!   ```text
//!   [u16 BE length][opus packet bytes] [u16 BE length][opus packet bytes] ...
//!   ```
//!
//!   Each packet covers 20 ms of mono audio at 48 kHz, encoded with
//!   `Application::Voip` at 12 kbps.
//!
//! - **Codec2** (`VoiceCodec::Codec2`, `codec_param = mode in 0..=5`):
//!   raw concatenated packed frames of the mode's fixed size
//!   (`bits_per_frame / 8`, rounded up). 8 kHz mono internally.
//!
//! - **AMR-NB** (`VoiceCodec::AmrNb`, `codec_param = mode in 0..=7`):
//!   concatenated IF1 storage-format frames. Each frame is a 1-byte
//!   ToC header (encoding the mode in bits 3..6) followed by the
//!   mode-specific number of speech bytes, for totals (incl. ToC) of
//!   13/14/16/18/20/21/27/32 bytes per 20 ms frame. 8 kHz mono internally.
//!   The actual encode/decode work goes through `libopencore-amrnb` over
//!   raw FFI; no `-sys` crate.

use std::time::Duration;

use voicetastic_core::voice::VoiceCodec;

/// Sample rate used for both capture and playback for the Opus path, and
/// the rate the playback pipeline expects after [`decode_clip`].
#[allow(dead_code)] // unused when `audio` feature is off
pub const SAMPLE_RATE_HZ: u32 = 48_000;
/// Mono frame size (samples) corresponding to a 20 ms Opus packet at 48 kHz.
#[allow(dead_code)]
pub const FRAME_SAMPLES: usize = 960;
/// Sample rate Codec2 operates on (all modes).
#[allow(dead_code)]
pub const CODEC2_SAMPLE_RATE_HZ: u32 = 8_000;
/// Sample rate AMR-NB operates on (all modes).
#[allow(dead_code)]
pub const AMRNB_SAMPLE_RATE_HZ: u32 = 8_000;
/// Samples per AMR-NB frame (20 ms @ 8 kHz mono).
#[allow(dead_code)]
pub const AMRNB_SAMPLES_PER_FRAME: usize = 160;
/// Target Opus bitrate. 12 kbps voice keeps a 30 s clip under the
/// protocol's per-message size budget.
#[allow(dead_code)]
pub const OPUS_BITRATE: i32 = 12_000;

/// Errors surfaced by the audio path. Kept small so the UI can map them to
/// a user-facing status string without pattern matching every variant.
#[allow(dead_code)] // some variants only used under specific feature combos
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("audio support is disabled (rebuild with `--features audio`)")]
    FeatureDisabled,
    #[error("no default audio {0} device")]
    NoDevice(&'static str),
    #[error("audio device does not support a usable configuration")]
    UnsupportedConfig,
    #[error("audio backend error: {0}")]
    Backend(String),
    #[error("codec error: {0}")]
    Codec(String),
    #[error("recording produced no audio")]
    Empty,
    #[error("unsupported codec for playback/encoding: {0:?}")]
    UnsupportedCodec(VoiceCodec),
}

/// A finished recording, ready to feed to the voice protocol.
#[derive(Debug, Clone)]
pub struct RecordedClip {
    /// Encoded codec payload — see module docs for per-codec layout.
    pub payload: Vec<u8>,
    /// Codec identifier matching `voice::VoiceCodec`.
    pub codec: VoiceCodec,
    /// Codec-specific parameter byte (e.g. Codec2 mode index).
    pub codec_param: u8,
    pub duration: Duration,
}

/// `true` when the binary was built with `--features audio`.
pub const fn is_available() -> bool {
    cfg!(feature = "audio")
}

/// Number of Codec2 samples per encoded frame for each mode index `0..=5`.
const CODEC2_SAMPLES_PER_FRAME: [usize; 6] = [160, 160, 320, 320, 320, 320];
/// Packed bytes per Codec2 frame for each mode index `0..=5`.
const CODEC2_BYTES_PER_FRAME: [usize; 6] = [8, 6, 8, 7, 7, 6];

fn codec2_frame_sizes(mode: u8) -> Option<(usize, usize)> {
    let i = mode as usize;
    if i < CODEC2_SAMPLES_PER_FRAME.len() {
        Some((CODEC2_SAMPLES_PER_FRAME[i], CODEC2_BYTES_PER_FRAME[i]))
    } else {
        None
    }
}

/// Total bytes (ToC + speech) per AMR-NB frame for each mode index `0..=7`.
const AMRNB_BYTES_PER_FRAME: [usize; 8] = [13, 14, 16, 18, 20, 21, 27, 32];

/// Extract the AMR-NB mode index from an IF1 storage-format ToC byte.
/// The mode (FT field) lives in bits 3..6 of the ToC.
fn amrnb_mode_from_toc(toc: u8) -> u8 {
    (toc >> 3) & 0x0F
}

/// Total bytes (ToC + speech) of an AMR-NB frame, given a ToC byte.
/// Returns `None` for non-speech frame types (DTX/SID/no_data) since we
/// never emit them.
fn amrnb_frame_size_from_toc(toc: u8) -> Option<usize> {
    let mode = amrnb_mode_from_toc(toc) as usize;
    AMRNB_BYTES_PER_FRAME.get(mode).copied()
}

/// Best-effort estimate of the wall-clock duration of an encoded payload,
/// in milliseconds. Returns 0 for unknown codec parameters. Doesn't need
/// the `audio` feature — used by the chat watcher to label inbound clips
/// even on headless builds.
pub fn payload_duration_ms(payload: &[u8], codec: VoiceCodec, codec_param: u8) -> u32 {
    match codec {
        VoiceCodec::Opus => {
            let mut i = 0;
            let mut packets: u32 = 0;
            while i + 2 <= payload.len() {
                let len = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
                i += 2;
                if i + len > payload.len() {
                    break;
                }
                i += len;
                packets += 1;
            }
            packets * 20
        }
        VoiceCodec::Codec2 => {
            let Some((samples, bytes)) = codec2_frame_sizes(codec_param) else {
                return 0;
            };
            let frames = (payload.len() / bytes) as u32;
            frames * (samples as u32) / 8
        }
        VoiceCodec::AmrNb => {
            // Walk the IF1 ToC bytes — frames can in principle mix modes,
            // but we always emit a fixed mode.
            let mut i = 0;
            let mut frames: u32 = 0;
            while i < payload.len() {
                let Some(size) = amrnb_frame_size_from_toc(payload[i]) else {
                    break;
                };
                if i + size > payload.len() {
                    break;
                }
                i += size;
                frames += 1;
            }
            frames * 20
        }
        _ => 0,
    }
}

#[cfg(feature = "audio")]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    use codec2::{Codec2, Codec2Mode};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, SupportedStreamConfig};
    use opus::{Application, Bitrate, Channels, Decoder, Encoder};
    use parking_lot::Mutex;

    // Raw FFI bindings to libopencore-amrnb. No -sys crate exists on
    // crates.io for this library, so we declare the surface inline.
    // System package: `libopencore-amrnb-dev` (Debian/Ubuntu) or
    // `opencore-amr` (Arch).
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

    /// Owned AMR-NB encoder state. Calls `Encoder_Interface_exit` on drop.
    struct AmrnbEncoder(*mut c_void);
    impl AmrnbEncoder {
        fn new() -> Result<Self, AudioError> {
            // dtx=0 → always emit speech frames (no SID/no_data), which
            // keeps the wire format trivially walkable.
            // SAFETY: FFI call with no preconditions; checks for NULL.
            let p = unsafe { Encoder_Interface_init(0) };
            if p.is_null() {
                return Err(AudioError::Codec(
                    "amrnb: Encoder_Interface_init failed".into(),
                ));
            }
            Ok(Self(p))
        }
    }
    impl Drop for AmrnbEncoder {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: pointer came from Encoder_Interface_init and is
                // only freed here exactly once.
                unsafe { Encoder_Interface_exit(self.0) };
                self.0 = std::ptr::null_mut();
            }
        }
    }

    /// Owned AMR-NB decoder state. Calls `Decoder_Interface_exit` on drop.
    struct AmrnbDecoder(*mut c_void);
    impl AmrnbDecoder {
        fn new() -> Result<Self, AudioError> {
            // SAFETY: FFI call with no preconditions; checks for NULL.
            let p = unsafe { Decoder_Interface_init() };
            if p.is_null() {
                return Err(AudioError::Codec(
                    "amrnb: Decoder_Interface_init failed".into(),
                ));
            }
            Ok(Self(p))
        }
    }
    impl Drop for AmrnbDecoder {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: pointer came from Decoder_Interface_init and is
                // only freed here exactly once.
                unsafe { Decoder_Interface_exit(self.0) };
                self.0 = std::ptr::null_mut();
            }
        }
    }

    fn backend<E: std::fmt::Display>(e: E) -> AudioError {
        AudioError::Backend(e.to_string())
    }
    fn codec<E: std::fmt::Display>(e: E) -> AudioError {
        AudioError::Codec(e.to_string())
    }

    fn codec2_mode_from_byte(b: u8) -> Result<Codec2Mode, AudioError> {
        Ok(match b {
            0 => Codec2Mode::MODE_3200,
            1 => Codec2Mode::MODE_2400,
            2 => Codec2Mode::MODE_1600,
            3 => Codec2Mode::MODE_1400,
            4 => Codec2Mode::MODE_1300,
            5 => Codec2Mode::MODE_1200,
            _ => {
                return Err(AudioError::Codec(format!("unknown codec2 mode index {b}")));
            }
        })
    }

    fn amrnb_validate_mode(b: u8) -> Result<i32, AudioError> {
        if (b as usize) < AMRNB_BYTES_PER_FRAME.len() {
            Ok(b as i32)
        } else {
            Err(AudioError::Codec(format!("unknown amrnb mode index {b}")))
        }
    }

    fn pick_config(
        device: &cpal::Device,
        for_input: bool,
    ) -> Result<SupportedStreamConfig, AudioError> {
        if for_input {
            device.default_input_config().map_err(backend)
        } else {
            device.default_output_config().map_err(backend)
        }
    }

    /// Streaming linear resampler.
    struct Resampler {
        ratio: f64,
        cursor: f64,
        last: f32,
    }

    impl Resampler {
        fn new(src_hz: u32, dst_hz: u32) -> Self {
            Self {
                ratio: src_hz as f64 / dst_hz as f64,
                cursor: 0.0,
                last: 0.0,
            }
        }

        fn push(&mut self, input: &[f32], dst: &mut Vec<f32>) {
            if input.is_empty() {
                return;
            }
            let n = input.len() as f64;
            while self.cursor < n {
                let idx_floor = self.cursor.floor();
                let frac = (self.cursor - idx_floor) as f32;
                let i0 = idx_floor as isize;
                let s0 = if i0 < 0 {
                    self.last
                } else {
                    input[i0 as usize]
                };
                let s1 = if (i0 + 1) < 0 {
                    self.last
                } else {
                    input.get((i0 + 1) as usize).copied().unwrap_or(s0)
                };
                dst.push(s0 + (s1 - s0) * frac);
                self.cursor += self.ratio;
            }
            self.last = *input.last().unwrap();
            self.cursor -= n;
        }
    }

    fn to_mono_f32<T>(data: &[T], channels: usize, dst: &mut Vec<f32>)
    where
        T: cpal::SizedSample,
        f32: cpal::FromSample<T>,
    {
        if channels == 1 {
            dst.reserve(data.len());
            for s in data {
                dst.push((*s).to_sample::<f32>());
            }
        } else {
            for frame in data.chunks_exact(channels) {
                let mut acc = 0.0f32;
                for x in frame {
                    acc += (*x).to_sample::<f32>();
                }
                dst.push(acc / channels as f32);
            }
        }
    }

    /// Streaming encoder state.
    enum EncState {
        Opus {
            encoder: Encoder,
            scratch: Vec<f32>,
            payload: Vec<u8>,
        },
        Codec2 {
            // Boxed because `Codec2` carries ~7.8 kB of mode tables; storing
            // it inline blew up the enum size and tripped clippy's
            // `large_enum_variant`.
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
        fn new(codec_id: VoiceCodec, codec_param: u8) -> Result<Self, AudioError> {
            match codec_id {
                VoiceCodec::Opus => {
                    let mut enc = Encoder::new(SAMPLE_RATE_HZ, Channels::Mono, Application::Voip)
                        .map_err(codec)?;
                    enc.set_bitrate(Bitrate::Bits(OPUS_BITRATE))
                        .map_err(codec)?;
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
                other => Err(AudioError::UnsupportedCodec(other)),
            }
        }

        fn push(&mut self, src: &[f32]) -> Result<(), AudioError> {
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
                        let n = encoder.encode(&frame, &mut buf).map_err(codec)?;
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
                        // SAFETY: `enc.0` is a valid encoder state; speech
                        // pointer covers 160 i16 samples; serial covers >=32 bytes.
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
                            return Err(AudioError::Codec(format!(
                                "amrnb: Encoder_Interface_Encode returned {n}"
                            )));
                        }
                        let n = n as usize;
                        // With dtx=0 and a fixed mode the encoder always
                        // produces a full speech frame of the expected size.
                        if n != *bytes_per_frame {
                            return Err(AudioError::Codec(format!(
                                "amrnb: unexpected frame size {n}, want {bytes_per_frame}"
                            )));
                        }
                        payload.extend_from_slice(&serial[..n]);
                    }
                    Ok(())
                }
            }
        }

        fn finish(mut self) -> Result<Vec<u8>, AudioError> {
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
                        let n = encoder.encode(&frame, &mut buf).map_err(codec)?;
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
                        // SAFETY: see `push`; valid pointers and sizes.
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

    #[allow(dead_code)]
    pub struct Recorder {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<Result<RecordedClip, AudioError>>>,
        started_at: Instant,
        max: Duration,
    }

    impl Recorder {
        pub fn start(
            max_secs: u32,
            codec_id: VoiceCodec,
            codec_param: u8,
        ) -> Result<Self, AudioError> {
            // Fail fast on unsupported codecs.
            let _ = EncState::new(codec_id, codec_param)?;

            let max = Duration::from_secs(max_secs.max(1) as u64);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = Arc::clone(&stop);
            let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), AudioError>>(1);

            let thread = std::thread::Builder::new()
                .name("voicetastic-rec".into())
                .spawn(move || run_capture(stop_thread, max, codec_id, codec_param, ready_tx))
                .map_err(backend)?;

            match ready_rx.recv() {
                Ok(Ok(())) => Ok(Self {
                    stop,
                    thread: Some(thread),
                    started_at: Instant::now(),
                    max,
                }),
                Ok(Err(e)) => {
                    let _ = thread.join();
                    Err(e)
                }
                Err(_) => Err(AudioError::Backend("capture thread died".into())),
            }
        }

        pub fn elapsed(&self) -> Duration {
            self.started_at.elapsed()
        }

        pub fn finish(mut self) -> Result<RecordedClip, AudioError> {
            self.stop.store(true, Ordering::SeqCst);
            let h = self
                .thread
                .take()
                .ok_or_else(|| AudioError::Backend("recorder already consumed".into()))?;
            h.join()
                .map_err(|_| AudioError::Backend("capture thread panicked".into()))?
        }
    }

    impl Drop for Recorder {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.thread.take() {
                let _ = h.join();
            }
        }
    }

    type CaptureBuf = Arc<Mutex<Vec<f32>>>;

    fn build_input_stream(
        device: &cpal::Device,
        cfg: &SupportedStreamConfig,
        buffer: CaptureBuf,
    ) -> Result<cpal::Stream, AudioError> {
        let channels = cfg.channels() as usize;
        let src_hz = cfg.sample_rate().0;
        let resampler = Arc::new(Mutex::new(Resampler::new(src_hz, SAMPLE_RATE_HZ)));
        let stream_cfg: cpal::StreamConfig = cfg.clone().into();
        let err_cb = |e| tracing::warn!(?e, "input stream error");

        macro_rules! build {
            ($T:ty) => {{
                let rs = Arc::clone(&resampler);
                let buf = Arc::clone(&buffer);
                device
                    .build_input_stream(
                        &stream_cfg,
                        move |data: &[$T], _| {
                            let mut mono = Vec::with_capacity(data.len() / channels.max(1));
                            to_mono_f32::<$T>(data, channels, &mut mono);
                            let mut out_buf = buf.lock();
                            rs.lock().push(&mono, &mut out_buf);
                        },
                        err_cb,
                        None,
                    )
                    .map_err(backend)
            }};
        }
        match cfg.sample_format() {
            SampleFormat::F32 => build!(f32),
            SampleFormat::I16 => build!(i16),
            SampleFormat::U16 => build!(u16),
            other => Err(AudioError::Backend(format!(
                "unsupported input sample format {other:?}"
            ))),
        }
    }

    fn run_capture(
        stop: Arc<AtomicBool>,
        max: Duration,
        codec_id: VoiceCodec,
        codec_param: u8,
        ready: mpsc::SyncSender<Result<(), AudioError>>,
    ) -> Result<RecordedClip, AudioError> {
        let host = cpal::default_host();
        let device = match host.default_input_device() {
            Some(d) => d,
            None => {
                let _ = ready.send(Err(AudioError::NoDevice("input")));
                return Err(AudioError::NoDevice("input"));
            }
        };
        let cfg = match pick_config(&device, true) {
            Ok(c) => c,
            Err(e) => {
                let _ = ready.send(Err(AudioError::Backend(e.to_string())));
                return Err(e);
            }
        };
        tracing::info!(
            rate = cfg.sample_rate().0,
            channels = cfg.channels(),
            format = ?cfg.sample_format(),
            codec = ?codec_id,
            codec_param,
            "opening capture device"
        );

        let buffer: CaptureBuf = Arc::new(Mutex::new(Vec::<f32>::with_capacity(
            SAMPLE_RATE_HZ as usize,
        )));
        let stream = match build_input_stream(&device, &cfg, Arc::clone(&buffer)) {
            Ok(s) => s,
            Err(e) => {
                let _ = ready.send(Err(AudioError::Backend(e.to_string())));
                return Err(e);
            }
        };
        if let Err(e) = stream.play() {
            let err = backend(e);
            let _ = ready.send(Err(AudioError::Backend(err.to_string())));
            return Err(err);
        }
        let _ = ready.send(Ok(()));

        let mut enc = EncState::new(codec_id, codec_param)?;
        let mut total_48k_samples: usize = 0;
        let started = Instant::now();

        while !stop.load(Ordering::SeqCst) && started.elapsed() < max {
            std::thread::sleep(Duration::from_millis(20));
            let drained: Vec<f32> = {
                let mut b = buffer.lock();
                std::mem::take(&mut *b)
            };
            if !drained.is_empty() {
                total_48k_samples += drained.len();
                enc.push(&drained)?;
            }
        }

        drop(stream);
        let tail: Vec<f32> = {
            let mut b = buffer.lock();
            std::mem::take(&mut *b)
        };
        if !tail.is_empty() {
            total_48k_samples += tail.len();
            enc.push(&tail)?;
        }

        let payload = enc.finish()?;
        if total_48k_samples == 0 || payload.is_empty() {
            return Err(AudioError::Empty);
        }

        let duration =
            Duration::from_micros((total_48k_samples as u64 * 1_000_000) / SAMPLE_RATE_HZ as u64);
        Ok(RecordedClip {
            payload,
            codec: codec_id,
            codec_param,
            duration,
        })
    }

    /// Decode an encoded payload to 48 kHz mono i16 PCM.
    pub fn decode_clip(
        payload: &[u8],
        codec_id: VoiceCodec,
        codec_param: u8,
    ) -> Result<Vec<i16>, AudioError> {
        match codec_id {
            VoiceCodec::Opus => decode_opus(payload),
            VoiceCodec::Codec2 => decode_codec2(payload, codec_param),
            VoiceCodec::AmrNb => decode_amrnb(payload),
            other => Err(AudioError::UnsupportedCodec(other)),
        }
    }

    fn decode_opus(payload: &[u8]) -> Result<Vec<i16>, AudioError> {
        let mut dec = Decoder::new(SAMPLE_RATE_HZ, Channels::Mono).map_err(codec)?;
        let mut pcm: Vec<i16> = Vec::new();
        let mut i = 0;
        let mut frame = [0i16; FRAME_SAMPLES];
        while i + 2 <= payload.len() {
            let len = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
            i += 2;
            if i + len > payload.len() {
                return Err(AudioError::Codec("truncated opus stream".into()));
            }
            let pkt = &payload[i..i + len];
            i += len;
            let n = dec.decode(pkt, &mut frame[..], false).map_err(codec)?;
            pcm.extend_from_slice(&frame[..n]);
        }
        Ok(pcm)
    }

    fn decode_codec2(payload: &[u8], codec_param: u8) -> Result<Vec<i16>, AudioError> {
        let mode = codec2_mode_from_byte(codec_param)?;
        let mut c2 = Codec2::new(mode);
        let samples_per_frame = c2.samples_per_frame();
        let bytes_per_frame = c2.bits_per_frame().div_ceil(8);
        if bytes_per_frame == 0 {
            return Err(AudioError::Codec("codec2: zero-size frame".into()));
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
        // Upsample 8 kHz → 48 kHz for the playback pipeline.
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

    fn decode_amrnb(payload: &[u8]) -> Result<Vec<i16>, AudioError> {
        let dec = AmrnbDecoder::new()?;
        let mut pcm8k_i16: Vec<i16> = Vec::new();
        let mut i = 0;
        let mut frame = [0i16; AMRNB_SAMPLES_PER_FRAME];
        while i < payload.len() {
            let Some(size) = amrnb_frame_size_from_toc(payload[i]) else {
                return Err(AudioError::Codec(format!(
                    "amrnb: unsupported ToC byte {:#x}",
                    payload[i]
                )));
            };
            if i + size > payload.len() {
                return Err(AudioError::Codec("amrnb: truncated frame".into()));
            }
            // SAFETY: `dec.0` is a valid decoder state; input covers `size`
            // bytes; output covers 160 i16 samples.
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

    /// Shared progress counters readable from any thread.
    pub struct PlaybackProgress {
        pos: AtomicUsize,
        total: AtomicUsize,
        rate: AtomicU32,
    }

    impl PlaybackProgress {
        fn new() -> Self {
            Self {
                pos: AtomicUsize::new(0),
                total: AtomicUsize::new(0),
                rate: AtomicU32::new(SAMPLE_RATE_HZ),
            }
        }
        pub fn snapshot(&self) -> (Duration, Duration) {
            let pos = self.pos.load(Ordering::Relaxed);
            let total = self.total.load(Ordering::Relaxed);
            let rate = self.rate.load(Ordering::Relaxed).max(1) as u64;
            let dur = |samples: usize| Duration::from_micros((samples as u64 * 1_000_000) / rate);
            (dur(pos.min(total)), dur(total))
        }
    }

    pub struct PlaybackHandle {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
        progress: Arc<PlaybackProgress>,
    }

    impl PlaybackHandle {
        pub fn stop(mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.thread.take() {
                let _ = h.join();
            }
        }

        pub fn progress(&self) -> (Duration, Duration) {
            self.progress.snapshot()
        }

        pub fn is_finished(&self) -> bool {
            let (e, t) = self.progress.snapshot();
            !t.is_zero() && e >= t
        }
    }

    impl Drop for PlaybackHandle {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.thread.take() {
                let _ = h.join();
            }
        }
    }

    pub fn play_clip(
        payload: &[u8],
        codec_id: VoiceCodec,
        codec_param: u8,
    ) -> Result<PlaybackHandle, AudioError> {
        let pcm_i16 = decode_clip(payload, codec_id, codec_param)?;
        if pcm_i16.is_empty() {
            return Err(AudioError::Empty);
        }
        let pcm_f32: Vec<f32> = pcm_i16
            .into_iter()
            .map(|s| s as f32 / i16::MAX as f32)
            .collect();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let progress = Arc::new(PlaybackProgress::new());
        let progress_thread = Arc::clone(&progress);
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), AudioError>>(1);
        let ready_for_thread = ready_tx.clone();
        let thread = std::thread::Builder::new()
            .name("voicetastic-play".into())
            .spawn(move || {
                if let Err(e) =
                    run_playback(stop_thread, pcm_f32, progress_thread, &ready_for_thread)
                {
                    let _ = ready_for_thread.send(Err(e));
                }
            })
            .map_err(backend)?;
        drop(ready_tx);

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(PlaybackHandle {
                stop,
                thread: Some(thread),
                progress,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(AudioError::Backend("playback thread died".into())),
        }
    }

    fn resample_to(pcm: &[f32], dst_hz: u32) -> Vec<f32> {
        if dst_hz == SAMPLE_RATE_HZ {
            return pcm.to_vec();
        }
        let mut out = Vec::with_capacity(pcm.len() * dst_hz as usize / SAMPLE_RATE_HZ as usize + 1);
        let mut rs = Resampler::new(SAMPLE_RATE_HZ, dst_hz);
        rs.push(pcm, &mut out);
        out
    }

    fn build_output_stream(
        device: &cpal::Device,
        cfg: &SupportedStreamConfig,
        pcm: Arc<Vec<f32>>,
        cursor: Arc<AtomicUsize>,
    ) -> Result<cpal::Stream, AudioError> {
        let channels = cfg.channels() as usize;
        let stream_cfg: cpal::StreamConfig = cfg.clone().into();
        let err_cb = |e| tracing::warn!(?e, "output stream error");

        macro_rules! build {
            ($T:ty) => {{
                let pcm = Arc::clone(&pcm);
                let cur = Arc::clone(&cursor);
                device
                    .build_output_stream(
                        &stream_cfg,
                        move |out: &mut [$T], _| {
                            for frame in out.chunks_mut(channels) {
                                let idx = cur.fetch_add(1, Ordering::Relaxed);
                                let s = pcm.get(idx).copied().unwrap_or(0.0);
                                let v: $T = <$T as cpal::FromSample<f32>>::from_sample_(s);
                                for ch in frame {
                                    *ch = v;
                                }
                            }
                        },
                        err_cb,
                        None,
                    )
                    .map_err(backend)
            }};
        }
        match cfg.sample_format() {
            SampleFormat::F32 => build!(f32),
            SampleFormat::I16 => build!(i16),
            SampleFormat::U16 => build!(u16),
            other => Err(AudioError::Backend(format!(
                "unsupported output sample format {other:?}"
            ))),
        }
    }

    fn run_playback(
        stop: Arc<AtomicBool>,
        pcm_48k: Vec<f32>,
        progress: Arc<PlaybackProgress>,
        ready: &mpsc::SyncSender<Result<(), AudioError>>,
    ) -> Result<(), AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(AudioError::NoDevice("output"))?;
        let cfg = pick_config(&device, false)?;
        tracing::info!(
            rate = cfg.sample_rate().0,
            channels = cfg.channels(),
            format = ?cfg.sample_format(),
            "opening playback device"
        );

        let resampled = resample_to(&pcm_48k, cfg.sample_rate().0);
        let total = resampled.len();
        let pcm = Arc::new(resampled);
        let cursor = Arc::new(AtomicUsize::new(0));
        progress.total.store(total, Ordering::Relaxed);
        progress.rate.store(cfg.sample_rate().0, Ordering::Relaxed);

        let stream = build_output_stream(&device, &cfg, Arc::clone(&pcm), Arc::clone(&cursor))?;
        stream.play().map_err(backend)?;
        let _ = ready.send(Ok(()));

        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let pos = cursor.load(Ordering::Relaxed);
            progress.pos.store(pos, Ordering::Relaxed);
            if pos >= total {
                std::thread::sleep(Duration::from_millis(80));
                progress.pos.store(total, Ordering::Relaxed);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(stream);
        Ok(())
    }
}

#[cfg(not(feature = "audio"))]
mod imp {
    use super::*;

    pub struct Recorder;
    impl Recorder {
        pub fn start(
            _max_secs: u32,
            _codec: VoiceCodec,
            _codec_param: u8,
        ) -> Result<Self, AudioError> {
            Err(AudioError::FeatureDisabled)
        }
        pub fn elapsed(&self) -> Duration {
            Duration::ZERO
        }
        pub fn finish(self) -> Result<RecordedClip, AudioError> {
            Err(AudioError::FeatureDisabled)
        }
    }

    pub struct PlaybackHandle;
    impl PlaybackHandle {
        pub fn stop(self) {}
        pub fn progress(&self) -> (Duration, Duration) {
            (Duration::ZERO, Duration::ZERO)
        }
        pub fn is_finished(&self) -> bool {
            true
        }
    }

    pub fn play_clip(
        _payload: &[u8],
        _codec: VoiceCodec,
        _codec_param: u8,
    ) -> Result<PlaybackHandle, AudioError> {
        Err(AudioError::FeatureDisabled)
    }
}

pub use imp::{PlaybackHandle, Recorder, play_clip};
