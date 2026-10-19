use pm_model::AudioFormat;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("cannot open {path}: {source}")]
    Open { path: PathBuf, source: std::io::Error },
    #[error("unsupported file: {0}")]
    Unsupported(String),
    #[error("malformed file: {0}")]
    Malformed(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("seek failed: {0}")]
    Seek(String),
    #[error("unsupported audio format {0}")]
    UnsupportedFormat(AudioFormat),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Pipeline(#[from] crate::pipeline::PipelineError),
    #[error(transparent)]
    Sink(#[from] pm_zones::SinkError),
}

impl From<symphonia::core::errors::Error> for AudioError {
    fn from(e: symphonia::core::errors::Error) -> Self {
        use symphonia::core::errors::Error as E;
        match e {
            E::IoError(io) => AudioError::Io(io),
            E::DecodeError(m) => AudioError::Decode(m.to_string()),
            E::SeekError(k) => AudioError::Seek(k.as_str().to_string()),
            E::Unsupported(m) => AudioError::Unsupported(m.to_string()),
            E::LimitError(m) => AudioError::Malformed(m.to_string()),
            E::ResetRequired => AudioError::Decode("decoder reset required".into()),
        }
    }
}
