//! Microphone capture and speaker playback for voice messages.
//!
//! Codec encode/decode lives in [`voicetastic_core::codec`]; this module owns
//! only the cpal-based audio I/O (input stream → [`Encoder`] → `RecordedClip`,
//! and `RecordedClip` → [`decode`](voicetastic_core::codec::decode) → output
//! stream).
//!
//! Gated behind the `audio` Cargo feature. With the feature off the module
//! exposes the same surface but every entry point returns
//! [`AudioError::FeatureDisabled`] and [`is_available`] is `false`.

use std::time::Duration;

use voicetastic_core::codec::CodecError;
use voicetastic_core::voice::VoiceCodec;

/// Re-exported so existing import sites (`crate::audio::*`) keep working.
pub use voicetastic_core::codec::{OpusBandwidth, RecordedClip, is_available, payload_duration_ms};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors surfaced by the audio I/O path.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("audio support is disabled (rebuild with `--features audio`)")]
    #[allow(dead_code)]
    FeatureDisabled,
    #[error("no default audio {0} device")]
    NoDevice(&'static str),
    #[error("audio backend error: {0}")]
    Backend(String),
    #[error("codec error: {0}")]
    Codec(#[from] CodecError),
}

// ---------------------------------------------------------------------------
// Feature-gated implementation
// ---------------------------------------------------------------------------

#[cfg(feature = "audio")]
mod imp {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, SupportedStreamConfig};
    use parking_lot::Mutex;

    use voicetastic_core::codec::{self, Encoder, Resampler, SAMPLE_RATE_HZ};

    // ------------------------------------------------------------------
    // Input helpers
    // ------------------------------------------------------------------

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
        opus_bw: OpusBandwidth,
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

        let mut enc = Encoder::new(codec_id, codec_param, opus_bw)?;
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
            return Err(AudioError::Codec(CodecError::Empty));
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

    // ------------------------------------------------------------------
    // Recorder
    // ------------------------------------------------------------------

    pub struct Recorder {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<Result<RecordedClip, AudioError>>>,
        started_at: Instant,
    }

    impl Recorder {
        pub fn start(
            max_secs: u32,
            codec_id: VoiceCodec,
            codec_param: u8,
            opus_bw: OpusBandwidth,
        ) -> Result<Self, AudioError> {
            let _ = Encoder::new(codec_id, codec_param, opus_bw)?;

            let max = Duration::from_secs(max_secs.max(1) as u64);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = Arc::clone(&stop);
            let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), AudioError>>(1);

            let thread = std::thread::Builder::new()
                .name("voicetastic-rec".into())
                .spawn(move || {
                    run_capture(stop_thread, max, codec_id, codec_param, opus_bw, ready_tx)
                })
                .map_err(backend)?;

            match ready_rx.recv() {
                Ok(Ok(())) => Ok(Self {
                    stop,
                    thread: Some(thread),
                    started_at: Instant::now(),
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

    // ------------------------------------------------------------------
    // Playback
    // ------------------------------------------------------------------

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

    pub fn play_clip(
        payload: &[u8],
        codec_id: VoiceCodec,
        codec_param: u8,
    ) -> Result<PlaybackHandle, AudioError> {
        let pcm_i16 = codec::decode(payload, codec_id, codec_param)?;
        if pcm_i16.is_empty() {
            return Err(AudioError::Codec(CodecError::Empty));
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

    // ------------------------------------------------------------------
    // Playback helpers (visible outside imp via pub use)
    // ------------------------------------------------------------------

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

    fn backend<E: std::fmt::Display>(e: E) -> AudioError {
        AudioError::Backend(e.to_string())
    }
}

#[cfg(feature = "audio")]
pub use imp::*;

// ---------------------------------------------------------------------------
// Stubs when `audio` feature is off
// ---------------------------------------------------------------------------

#[cfg(not(feature = "audio"))]
mod imp {
    use super::*;

    pub struct Recorder;
    impl Recorder {
        pub fn start(
            _max_secs: u32,
            _codec: VoiceCodec,
            _codec_param: u8,
            _opus_bw: OpusBandwidth,
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

#[cfg(not(feature = "audio"))]
pub use imp::*;
