use thiserror::Error;

use crate::voice::VoiceCodec;

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("codec support is disabled (rebuild with `--features codecs`)")]
    FeatureDisabled,
    #[error("codec error: {0}")]
    Codec(String),
    #[error("unsupported codec: {0:?}")]
    UnsupportedCodec(VoiceCodec),
    #[error("encoding produced no audio")]
    Empty,
}
