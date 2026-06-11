/// Streaming resampler.
///
/// Downsampling paths (src_hz > dst_hz) apply a 63-tap Hamming-windowed sinc
/// FIR low-pass filter before interpolation to suppress aliasing. Upsampling
/// paths use plain linear interpolation (imaging artefacts are negligible
/// compared to 6:1 decimation aliasing).
///
/// The `push` signature is unchanged from the previous version so all 7 call
/// sites in `imp.rs` continue to work without modification.
use std::f64::consts::PI;

const TAPS: usize = 63;

pub struct Resampler {
    ratio: f64, // src_hz / dst_hz
    cursor: f64,
    /// Anti-alias FIR state. `None` when upsampling (ratio <= 1.0).
    fir: Option<FirState>,
}

struct FirState {
    /// Hamming-windowed sinc coefficients; DC gain normalised to 1.
    coeffs: [f32; TAPS],
    /// Last `TAPS - 1` input samples from the previous `push` call; zero-
    /// initialised. Keeps the filter state continuous across streaming chunks.
    history: Vec<f32>,
    /// Scratch buffer reused on every `push`: `[history, current_input]`.
    scratch: Vec<f32>,
}

/// Build a 63-tap Hamming-windowed sinc low-pass FIR at normalised cutoff
/// `cutoff` (fraction of the input sample rate, range 0..0.5). The DC gain is
/// normalised to 1.0 after windowing to avoid level drift.
fn hamming_sinc_coeffs(cutoff: f64) -> [f32; TAPS] {
    let m = (TAPS - 1) as f64 * 0.5; // centre tap index = 31.0
    let mut h = [0.0f64; TAPS];
    for (k, v) in h.iter_mut().enumerate() {
        let x = 2.0 * cutoff * (k as f64 - m);
        let sinc = if x.abs() < 1e-12 {
            1.0
        } else {
            (PI * x).sin() / (PI * x)
        };
        let hamming = 0.54 - 0.46 * (2.0 * PI * k as f64 / (TAPS - 1) as f64).cos();
        *v = 2.0 * cutoff * sinc * hamming;
    }
    let norm: f64 = h.iter().sum();
    let mut out = [0.0f32; TAPS];
    for (o, &v) in out.iter_mut().zip(h.iter()) {
        *o = (v / norm) as f32;
    }
    out
}

/// Evaluate the FIR at input position `i`.
///
/// `scratch` = `[history (TAPS-1 samples), input (M samples)]`.
/// Position `i` maps to `scratch[i + TAPS - 1]` (current) through
/// `scratch[i]` (oldest), giving a causal linear-phase filter.
#[inline]
fn fir_eval(scratch: &[f32], coeffs: &[f32; TAPS], i: usize) -> f32 {
    let base = i + TAPS - 1;
    let mut s = 0.0f32;
    for j in 0..TAPS {
        s += coeffs[j] * scratch[base - j];
    }
    s
}

impl Resampler {
    pub fn new(src_hz: u32, dst_hz: u32) -> Self {
        let ratio = src_hz as f64 / dst_hz as f64;
        let fir = if ratio > 1.0 {
            // Cutoff at 0.45 * (dst/src): 10 % guard band below the output
            // Nyquist (0.5 * dst/src). For 48 kHz -> 8 kHz: cutoff ≈ 3600 Hz.
            let cutoff = 0.45 * dst_hz as f64 / src_hz as f64;
            let coeffs = hamming_sinc_coeffs(cutoff);
            let mut history = Vec::with_capacity(TAPS - 1);
            history.resize(TAPS - 1, 0.0f32);
            Some(FirState {
                coeffs,
                history,
                scratch: Vec::new(),
            })
        } else {
            None
        };
        Self {
            ratio,
            cursor: 0.0,
            fir,
        }
    }

    /// Resample `input` and *append* the output samples to `dst`.
    ///
    /// `dst` is appended-to, never cleared: pass the same buffer across calls
    /// (after draining what was consumed) so steady-state pushes don't
    /// reallocate. Callers in the codec pipeline construct it once with
    /// `Vec::with_capacity(samples_per_frame * N)` and reuse it for the
    /// lifetime of the encode/decode session.
    pub fn push(&mut self, input: &[f32], dst: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let m = input.len();
        let n = m as f64;

        if let Some(fir) = &mut self.fir {
            // Build extended context [history (TAPS-1 samples), input].
            // scratch[i + TAPS - 1] = input[i] for i in 0..m.
            fir.scratch.clear();
            fir.scratch.extend_from_slice(&fir.history);
            fir.scratch.extend_from_slice(input);

            let mut prev_i = usize::MAX;
            let mut f0 = 0.0f32;

            while self.cursor < n {
                let i = self.cursor.floor() as usize;
                if i != prev_i {
                    f0 = fir_eval(&fir.scratch, &fir.coeffs, i);
                    prev_i = i;
                }
                let f1 = if i + 1 < m {
                    fir_eval(&fir.scratch, &fir.coeffs, i + 1)
                } else {
                    f0
                };
                let frac = (self.cursor - i as f64) as f32;
                dst.push(f0 + (f1 - f0) * frac);
                self.cursor += self.ratio;
            }

            // Update history: last TAPS-1 samples of input, for the next push.
            let hist = TAPS - 1;
            if m >= hist {
                fir.history.clear();
                fir.history.extend_from_slice(&input[m - hist..]);
            } else {
                // Input shorter than history length: shift left and append.
                fir.history.drain(..m);
                fir.history.extend_from_slice(input);
            }
        } else {
            // Upsampling: plain linear interpolation, no pre-filter needed.
            while self.cursor < n {
                let i = self.cursor.floor() as usize;
                let frac = (self.cursor - i as f64) as f32;
                let s0 = input[i];
                let s1 = input.get(i + 1).copied().unwrap_or(s0);
                dst.push(s0 + (s1 - s0) * frac);
                self.cursor += self.ratio;
            }
        }

        self.cursor -= n;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum: f32 = samples.iter().map(|x| x * x).sum();
        (sum / samples.len() as f32).sqrt()
    }

    /// Goertzel DFT bin magnitude squared, normalised by N^2 so the result
    /// is comparable across different-length signals.
    fn goertzel_power_norm(samples: &[f32], freq_hz: f32, sample_rate_hz: u32) -> f32 {
        let n = samples.len() as f32;
        let k = (0.5 + n * freq_hz / sample_rate_hz as f32) as usize;
        let omega = 2.0 * std::f32::consts::PI * k as f32 / n;
        let coeff = 2.0 * omega.cos();
        let (mut s2, mut s1) = (0.0f32, 0.0f32);
        for &x in samples {
            let s = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        let p = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        p / (n * n)
    }

    fn sine(freq_hz: f32, sample_rate_hz: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                (2.0 * std::f32::consts::PI * freq_hz * i as f32 / sample_rate_hz as f32).sin()
            })
            .collect()
    }

    // --- Alias band attenuation ---

    #[test]
    fn alias_band_attenuation_ge_30db() {
        // 12 kHz input is well into the stopband (cutoff ≈ 3.6 kHz for 48->8 kHz).
        // Without the FIR it would alias strongly; with it the output should be
        // at least 30 dB below the input.
        let src = 48_000u32;
        let dst = 8_000u32;
        let input = sine(12_000.0, src, 9600); // 200 ms
        let mut rs = Resampler::new(src, dst);
        let mut output = Vec::new();
        rs.push(&input, &mut output);
        let in_rms = rms(&input);
        let out_rms = rms(&output);
        // 30 dB => linear ratio 0.0316
        assert!(
            out_rms < in_rms * 0.0316,
            "alias not suppressed: in_rms={in_rms:.4} out_rms={out_rms:.4} ratio={:.4}",
            out_rms / in_rms
        );
    }

    // --- Passband preservation ---

    #[test]
    fn passband_within_1db() {
        // 1 kHz is well inside the passband. The Goertzel bin power should be
        // at least 10^(-1/10) ≈ 0.794 of the input power at the same frequency.
        let src = 48_000u32;
        let dst = 8_000u32;
        let n_in = 9600usize; // 200 ms at 48 kHz
        let input = sine(1_000.0, src, n_in);
        let mut rs = Resampler::new(src, dst);
        let mut output = Vec::new();
        rs.push(&input, &mut output);
        let in_power = goertzel_power_norm(&input, 1_000.0, src);
        let out_power = goertzel_power_norm(&output, 1_000.0, dst);
        // Output power within 1 dB of input: ratio > 10^(-1/10) ≈ 0.794
        assert!(
            out_power > in_power * 0.794,
            "passband > 1 dB loss: in_power={in_power:.6} out_power={out_power:.6}"
        );
    }

    // --- Streaming continuity (one-shot == streaming) ---

    #[test]
    fn streaming_equals_one_shot() {
        let src = 48_000u32;
        let dst = 8_000u32;
        let total = 2880usize; // 3 × 960-sample frames
        let chunk = 960usize;
        let input = sine(440.0, src, total);

        let mut rs_one = Resampler::new(src, dst);
        let mut one_shot = Vec::new();
        rs_one.push(&input, &mut one_shot);

        let mut rs_str = Resampler::new(src, dst);
        let mut streaming = Vec::new();
        for start in (0..total).step_by(chunk) {
            rs_str.push(&input[start..start + chunk], &mut streaming);
        }

        assert_eq!(one_shot.len(), streaming.len(), "length mismatch");
        let max_err = one_shot
            .iter()
            .zip(streaming.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_err < 1e-5,
            "streaming and one-shot diverge: max_err={max_err:.2e}"
        );
    }

    // --- Upsample output length ---

    #[test]
    fn upsample_length_ratio() {
        let input = vec![0.0f32; 100];
        let mut rs = Resampler::new(8_000, 48_000);
        let mut output = Vec::new();
        rs.push(&input, &mut output);
        // 100 samples × (48000/8000) = 600
        assert_eq!(output.len(), 600, "expected 600 output samples");
    }

    // --- Downsampling length ---

    #[test]
    fn downsample_length_ratio() {
        let input = vec![0.0f32; 960]; // one 20ms frame at 48 kHz
        let mut rs = Resampler::new(48_000, 8_000);
        let mut output = Vec::new();
        rs.push(&input, &mut output);
        // 960 / 6 = 160 samples at 8 kHz
        assert_eq!(output.len(), 160, "expected 160 output samples");
    }
}
