//! Codec2 encode/decode helpers exposed to Kotlin.
//!
//! Pure Rust — no libcodec2/FFI — so this just cross-compiles like any
//! other Rust crate for `aarch64-linux-android` and `x86_64-linux-android`.
//!
//! # Wire format
//!
//! `Codec2Encoder` produces, and `codec2_decode` consumes, the same
//! "raw concatenated packed frames" layout that `voicetastic-core`'s
//! `codec` module documents for `VoiceCodec::Codec2` — so the bytes
//! produced here can be fed straight into `build_message(...)` and the
//! receiver's `codec2_decode(...)` will recover the PCM.
//!
//! # Modes
//!
//! Mode index `u8` mirrors `codec2::Codec2Mode` (and the GUI's
//! `VoiceCodec2Mode`):
//!
//! | mode | bitrate | samples/frame | bytes/frame |
//! |------|---------|---------------|-------------|
//! | 0    | 3200    | 160           | 8           |
//! | 1    | 2400    | 160           | 6           |
//! | 2    | 1600    | 320           | 8           |
//! | 3    | 1400    | 320           | 7           |
//! | 4    | 1300    | 320           | 7           |
//! | 5    | 1200    | 320           | 6           |
//!
//! All modes are 8 kHz mono. The Android recorder is expected to deliver
//! `AudioRecord` PCM 16-bit at 8000 Hz.

use codec2::{Codec2, Codec2Mode};

#[derive(Debug, thiserror::Error)]
pub enum Codec2Error {
    #[error("invalid Codec2 mode (must be 0..=5)")]
    BadMode,
    #[error("input PCM length must be a multiple of samples_per_frame")]
    BadPcmLen,
    #[error("input payload length must be a multiple of bytes_per_frame")]
    BadPayloadLen,
}

fn mode_from_u8(m: u8) -> Result<Codec2Mode, Codec2Error> {
    Ok(match m {
        0 => Codec2Mode::MODE_3200,
        1 => Codec2Mode::MODE_2400,
        2 => Codec2Mode::MODE_1600,
        3 => Codec2Mode::MODE_1400,
        4 => Codec2Mode::MODE_1300,
        5 => Codec2Mode::MODE_1200,
        _ => return Err(Codec2Error::BadMode),
    })
}

/// Stateful Codec2 encoder. Kotlin pushes whole-frame PCM buffers and gets
/// back packed Codec2 bytes. The encoder state is retained between calls
/// so consecutive frames share inter-frame predictor history.
pub struct Codec2Encoder {
    inner: parking_lot::Mutex<Codec2>,
    samples_per_frame: u32,
    bytes_per_frame: u32,
}

impl Codec2Encoder {
    pub fn new(mode: u8) -> Result<Self, Codec2Error> {
        let m = mode_from_u8(mode)?;
        let c2 = Codec2::new(m);
        let spf = c2.samples_per_frame() as u32;
        let bpf = c2.bits_per_frame().div_ceil(8) as u32;
        Ok(Self {
            inner: parking_lot::Mutex::new(c2),
            samples_per_frame: spf,
            bytes_per_frame: bpf,
        })
    }

    pub fn samples_per_frame(&self) -> u32 {
        self.samples_per_frame
    }

    pub fn bytes_per_frame(&self) -> u32 {
        self.bytes_per_frame
    }

    /// Encode a buffer of 16-bit PCM samples. `pcm.len()` must be a
    /// multiple of `samples_per_frame()`. Returns packed bytes of length
    /// `(pcm.len() / samples_per_frame) * bytes_per_frame`.
    pub fn encode(&self, pcm: Vec<i16>) -> Result<Vec<u8>, Codec2Error> {
        let spf = self.samples_per_frame as usize;
        let bpf = self.bytes_per_frame as usize;
        if !pcm.len().is_multiple_of(spf) {
            return Err(Codec2Error::BadPcmLen);
        }
        let n_frames = pcm.len() / spf;
        let mut out = vec![0u8; n_frames * bpf];
        let mut c2 = self.inner.lock();
        for i in 0..n_frames {
            let pcm_slice = &pcm[i * spf..(i + 1) * spf];
            let out_slice = &mut out[i * bpf..(i + 1) * bpf];
            c2.encode(out_slice, pcm_slice);
        }
        Ok(out)
    }
}

/// One-shot decode of a complete Codec2 payload (concatenated packed
/// frames) into 16-bit PCM samples at 8 kHz mono. The codec is
/// constructed fresh, so this is safe to call repeatedly on full
/// messages without leaking state between them.
pub fn codec2_decode(payload: Vec<u8>, mode: u8) -> Result<Vec<i16>, Codec2Error> {
    let m = mode_from_u8(mode)?;
    let mut c2 = Codec2::new(m);
    let spf = c2.samples_per_frame();
    let bpf = c2.bits_per_frame().div_ceil(8);
    if !payload.len().is_multiple_of(bpf) {
        return Err(Codec2Error::BadPayloadLen);
    }
    let n_frames = payload.len() / bpf;
    let mut pcm = vec![0i16; n_frames * spf];
    for i in 0..n_frames {
        let bits_slice = &payload[i * bpf..(i + 1) * bpf];
        let pcm_slice = &mut pcm[i * spf..(i + 1) * spf];
        c2.decode(pcm_slice, bits_slice);
    }
    Ok(pcm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_silence_mode0() {
        let enc = Codec2Encoder::new(0).unwrap();
        let spf = enc.samples_per_frame() as usize;
        // 5 frames of silence
        let pcm = vec![0i16; spf * 5];
        let bytes = enc.encode(pcm).unwrap();
        assert_eq!(bytes.len() % enc.bytes_per_frame() as usize, 0);
        let pcm_back = codec2_decode(bytes, 0).unwrap();
        assert_eq!(pcm_back.len(), spf * 5);
    }

    #[test]
    fn rejects_bad_mode() {
        assert!(matches!(Codec2Encoder::new(99), Err(Codec2Error::BadMode)));
        assert!(matches!(
            codec2_decode(vec![], 99),
            Err(Codec2Error::BadMode)
        ));
    }

    #[test]
    fn rejects_bad_pcm_len() {
        let enc = Codec2Encoder::new(0).unwrap();
        // Not a multiple of samples_per_frame.
        let pcm = vec![0i16; (enc.samples_per_frame() as usize) + 1];
        assert!(matches!(enc.encode(pcm), Err(Codec2Error::BadPcmLen)));
    }

    #[test]
    fn rejects_bad_payload_len() {
        // Not a multiple of bytes_per_frame for mode 0 (8 bytes/frame).
        let bad = vec![0u8; 9];
        assert!(matches!(
            codec2_decode(bad, 0),
            Err(Codec2Error::BadPayloadLen)
        ));
    }
}
