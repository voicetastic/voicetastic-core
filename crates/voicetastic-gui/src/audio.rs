//! Microphone capture, Opus encode/decode and speaker playback for voice
//! messages composed in the Chat tab.
//!
//! Gated behind the `audio` Cargo feature so default builds remain free of
//! `cpal` (ALSA on Linux) and `audiopus` (libopus). With the feature off
//! the module exposes the same surface but every entry point returns an
//! [`AudioError::FeatureDisabled`] error and [`is_available`] is `false`.
//!
//! # Wire format
//!
//! [`RecordedClip::opus_stream`] is a sequence of length-prefixed Opus
//! packets:
//!
//! ```text
//! [u16 BE length][opus packet bytes] [u16 BE length][opus packet bytes] ...
//! ```
//!
//! Each packet covers 20 ms of mono audio at 48 kHz, encoded with
//! `Application::Voip` at 12 kbps. Receivers parse the same framing for
//! playback. The blob is what we hand to the codec-agnostic voice protocol
//! as the audio payload (codec=`Opus`, codec_param=0).

use std::time::Duration;

/// Sample rate used for both capture and playback.
#[allow(dead_code)] // unused when `audio` feature is off
pub const SAMPLE_RATE_HZ: u32 = 48_000;
/// Mono frame size (samples) corresponding to a 20 ms Opus packet at 48 kHz.
#[allow(dead_code)]
pub const FRAME_SAMPLES: usize = 960;
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
    #[error("audio device does not support a usable Opus configuration")]
    UnsupportedConfig,
    #[error("audio backend error: {0}")]
    Backend(String),
    #[error("opus codec error: {0}")]
    Codec(String),
    #[error("recording produced no audio")]
    Empty,
}

/// A finished recording, ready to feed to the voice protocol.
#[derive(Debug, Clone)]
pub struct RecordedClip {
    /// Length-prefixed Opus packets — see module docs.
    pub opus_stream: Vec<u8>,
    pub duration: Duration,
}

/// `true` when the binary was built with `--features audio`.
pub const fn is_available() -> bool {
    cfg!(feature = "audio")
}

#[cfg(feature = "audio")]
mod imp {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, SupportedStreamConfig};
    use opus::{Application, Bitrate, Channels, Decoder, Encoder};
    use parking_lot::Mutex;

    fn backend<E: std::fmt::Display>(e: E) -> AudioError {
        AudioError::Backend(e.to_string())
    }
    fn codec<E: std::fmt::Display>(e: E) -> AudioError {
        AudioError::Codec(e.to_string())
    }

    /// Pick the device's preferred config. We adapt to whatever sample
    /// rate, format and channel count it returns — Opus runs at 48 kHz
    /// mono internally and we resample / mix in software.
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

    /// Streaming linear resampler: f32 mono in → f32 mono out at a
    /// possibly-different rate. Voice (<4 kHz content) sounds fine with
    /// linear interp, and avoiding a proper polyphase filter keeps the
    /// dependency surface tiny.
    struct Resampler {
        ratio: f64, // src_hz / dst_hz — how many src samples per dst sample
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

        /// Push `input` source samples and append resampled output to `dst`.
        fn push(&mut self, input: &[f32], dst: &mut Vec<f32>) {
            if input.is_empty() {
                return;
            }
            // Concatenate `last` + input into a virtual buffer indexed by
            // [-1, input.len()).
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

    /// Convert an interleaved input buffer of arbitrary sample format and
    /// channel count to mono f32 in `[-1, 1]`.
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

    fn drain_frames(samples: &mut Vec<f32>) -> Vec<[i16; FRAME_SAMPLES]> {
        let mut out = Vec::new();
        while samples.len() >= FRAME_SAMPLES {
            let mut frame = [0i16; FRAME_SAMPLES];
            for (i, s) in samples.drain(..FRAME_SAMPLES).enumerate() {
                let clamped = s.clamp(-1.0, 1.0);
                frame[i] = (clamped * i16::MAX as f32) as i16;
            }
            out.push(frame);
        }
        out
    }

    fn encode_frame(
        encoder: &mut Encoder,
        frame: &[i16; FRAME_SAMPLES],
        dst: &mut Vec<u8>,
    ) -> Result<(), AudioError> {
        let mut buf = [0u8; 1275];
        let n = encoder.encode(frame, &mut buf).map_err(codec)?;
        dst.extend_from_slice(&(n as u16).to_be_bytes());
        dst.extend_from_slice(&buf[..n]);
        Ok(())
    }

    #[allow(dead_code)]
    pub struct Recorder {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<Result<RecordedClip, AudioError>>>,
        started_at: Instant,
        max: Duration,
    }

    impl Recorder {
        pub fn start(max_secs: u32) -> Result<Self, AudioError> {
            let max = Duration::from_secs(max_secs.max(1) as u64);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = Arc::clone(&stop);
            let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), AudioError>>(1);

            let thread = std::thread::Builder::new()
                .name("voicetastic-rec".into())
                .spawn(move || run_capture(stop_thread, max, ready_tx))
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

    /// Buffer of resampled 48 kHz mono f32 samples produced by the capture
    /// callback, drained by the encoder loop.
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

        let mut encoder =
            Encoder::new(SAMPLE_RATE_HZ, Channels::Mono, Application::Voip).map_err(codec)?;
        encoder
            .set_bitrate(Bitrate::Bits(OPUS_BITRATE))
            .map_err(codec)?;

        let mut opus_stream: Vec<u8> = Vec::new();
        let mut total_samples: usize = 0;
        let started = Instant::now();

        while !stop.load(Ordering::SeqCst) && started.elapsed() < max {
            std::thread::sleep(Duration::from_millis(20));
            let frames = {
                let mut b = buffer.lock();
                drain_frames(&mut b)
            };
            for frame in &frames {
                encode_frame(&mut encoder, frame, &mut opus_stream)?;
                total_samples += FRAME_SAMPLES;
            }
        }

        drop(stream);
        let mut tail = buffer.lock();
        if !tail.is_empty() {
            let cur_len = tail.len();
            tail.resize(cur_len.div_ceil(FRAME_SAMPLES) * FRAME_SAMPLES, 0.0);
            let frames = drain_frames(&mut tail);
            for frame in &frames {
                encode_frame(&mut encoder, frame, &mut opus_stream)?;
                total_samples += FRAME_SAMPLES;
            }
        }
        drop(tail);

        if total_samples == 0 || opus_stream.is_empty() {
            return Err(AudioError::Empty);
        }

        let duration =
            Duration::from_micros((total_samples as u64 * 1_000_000) / SAMPLE_RATE_HZ as u64);
        Ok(RecordedClip {
            opus_stream,
            duration,
        })
    }

    pub fn decode_clip(opus_stream: &[u8]) -> Result<Vec<i16>, AudioError> {
        let mut dec = Decoder::new(SAMPLE_RATE_HZ, Channels::Mono).map_err(codec)?;
        let mut pcm: Vec<i16> = Vec::new();
        let mut i = 0;
        let mut frame = [0i16; FRAME_SAMPLES];
        while i + 2 <= opus_stream.len() {
            let len = u16::from_be_bytes([opus_stream[i], opus_stream[i + 1]]) as usize;
            i += 2;
            if i + len > opus_stream.len() {
                return Err(AudioError::Codec("truncated opus stream".into()));
            }
            let pkt = &opus_stream[i..i + len];
            i += len;
            let n = dec.decode(pkt, &mut frame[..], false).map_err(codec)?;
            pcm.extend_from_slice(&frame[..n]);
        }
        Ok(pcm)
    }

    /// Shared progress counters readable from any thread. The cpal output
    /// callback bumps `pos` for every output sample; the UI polls
    /// `progress()` once per frame to drive the inline player.
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
        /// `(elapsed, total)` durations, derived from sample counts at the
        /// device sample rate. Saturates `elapsed` at `total`.
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

        /// `(elapsed, total)` since playback started.
        pub fn progress(&self) -> (Duration, Duration) {
            self.progress.snapshot()
        }

        /// `true` once the entire clip has been pushed to the device.
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

    pub fn play_clip(opus_stream: &[u8]) -> Result<PlaybackHandle, AudioError> {
        let pcm_i16 = decode_clip(opus_stream)?;
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

    /// Resample `pcm` (48 kHz mono f32) to `dst_hz` mono f32. Returns the
    /// full output buffer; voice clips are short, no need to stream this.
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
        pub fn start(_max_secs: u32) -> Result<Self, AudioError> {
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

    pub fn play_clip(_opus_stream: &[u8]) -> Result<PlaybackHandle, AudioError> {
        Err(AudioError::FeatureDisabled)
    }
}

pub use imp::{PlaybackHandle, Recorder, play_clip};
