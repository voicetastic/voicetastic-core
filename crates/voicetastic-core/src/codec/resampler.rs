/// Streaming linear resampler.
pub struct Resampler {
    ratio: f64,
    cursor: f64,
    last: f32,
}

impl Resampler {
    pub fn new(src_hz: u32, dst_hz: u32) -> Self {
        Self {
            ratio: src_hz as f64 / dst_hz as f64,
            cursor: 0.0,
            last: 0.0,
        }
    }

    /// Resample `input` and *append* the output samples to `dst`.
    ///
    /// `dst` is appended-to, never cleared: pass the same buffer across
    /// calls (after draining what was consumed) so steady-state pushes
    /// don't reallocate. Callers in the codec pipeline construct it once
    /// with `Vec::with_capacity(samples_per_frame * N)` and reuse it for
    /// the lifetime of the encode/decode session.
    pub fn push(&mut self, input: &[f32], dst: &mut Vec<f32>) {
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
        if let Some(&last) = input.last() {
            self.last = last;
        }
        self.cursor -= n;
    }
}
