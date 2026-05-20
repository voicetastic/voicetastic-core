//! Capture-side noise suppression.
//!
//! Wraps [`nnnoiseless`] (a pure-Rust port of Xiph's RNNoise) into a tiny
//! streaming API matching [`Encoder::push`](super::Encoder::push): callers
//! hand in arbitrary-length `f32` PCM at 48 kHz mono and pull cleaned PCM
//! back out, while we internally buffer to RNNoise's fixed 10 ms / 480-sample
//! frame size.
//!
//! Gated behind the `denoise` Cargo feature. Without the feature, [`Denoiser`]
//! is still constructible but its `process` is a passthrough — this keeps the
//! capture pipeline source-compatible regardless of build configuration.
//!
//! # Sample convention
//!
//! Our pipeline carries normalised `f32` in `[-1.0, 1.0]`, but RNNoise's
//! internal model expects i16-range floats. The wrapper scales by
//! `i16::MAX` on the way in and back out so the caller can stay in
//! normalised space.

#![allow(dead_code)]

/// RNNoise's fixed frame size (10 ms @ 48 kHz mono).
pub const DENOISE_FRAME_SIZE: usize = 480;

/// `true` when the library was built with `--features denoise`. When
/// `false`, [`Denoiser::process`] is a passthrough.
pub const fn denoise_available() -> bool {
    cfg!(feature = "denoise")
}

#[cfg(feature = "denoise")]
mod imp {
    use super::DENOISE_FRAME_SIZE;
    use nnnoiseless::DenoiseState;

    pub struct Denoiser {
        state: Box<DenoiseState<'static>>,
        in_buf: Vec<f32>,
        out_scratch: [f32; DENOISE_FRAME_SIZE],
    }

    impl Default for Denoiser {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Denoiser {
        pub fn new() -> Self {
            Self {
                state: DenoiseState::new(),
                in_buf: Vec::with_capacity(DENOISE_FRAME_SIZE * 2),
                out_scratch: [0.0; DENOISE_FRAME_SIZE],
            }
        }

        /// Push `src` (48 kHz mono, normalised `[-1.0, 1.0]`) and append
        /// every complete denoised 10 ms frame to `dst` (same convention).
        /// Tail samples shorter than [`DENOISE_FRAME_SIZE`] stay buffered
        /// until the next call or [`flush`](Self::flush).
        pub fn process(&mut self, src: &[f32], dst: &mut Vec<f32>) {
            // Scale into i16 range; RNNoise's model was trained on that.
            self.in_buf
                .extend(src.iter().map(|s| *s * f32::from(i16::MAX)));
            while self.in_buf.len() >= DENOISE_FRAME_SIZE {
                // `process_frame` writes into `out_scratch`; the input slice
                // is the first FRAME_SIZE elements of `in_buf`.
                let _voice_prob = self
                    .state
                    .process_frame(&mut self.out_scratch, &self.in_buf[..DENOISE_FRAME_SIZE]);
                self.in_buf.drain(..DENOISE_FRAME_SIZE);
                dst.extend(
                    self.out_scratch
                        .iter()
                        .map(|s| (*s / f32::from(i16::MAX)).clamp(-1.0, 1.0)),
                );
            }
        }

        /// Zero-pad and emit any buffered tail. Call once at end-of-stream
        /// so the trailing <10 ms of audio isn't dropped.
        pub fn flush(&mut self, dst: &mut Vec<f32>) {
            if self.in_buf.is_empty() {
                return;
            }
            self.in_buf.resize(DENOISE_FRAME_SIZE, 0.0);
            let _ = self
                .state
                .process_frame(&mut self.out_scratch, &self.in_buf[..DENOISE_FRAME_SIZE]);
            self.in_buf.clear();
            dst.extend(
                self.out_scratch
                    .iter()
                    .map(|s| (*s / f32::from(i16::MAX)).clamp(-1.0, 1.0)),
            );
        }
    }
}

#[cfg(not(feature = "denoise"))]
mod imp {
    pub struct Denoiser;

    impl Default for Denoiser {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Denoiser {
        pub fn new() -> Self {
            Self
        }

        pub fn process(&mut self, src: &[f32], dst: &mut Vec<f32>) {
            dst.extend_from_slice(src);
        }

        pub fn flush(&mut self, _dst: &mut Vec<f32>) {}
    }
}

pub use imp::Denoiser;
