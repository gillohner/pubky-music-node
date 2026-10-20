use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unsupported file: {0}")]
    Unsupported(PathBuf),
    #[error("cannot read {path}: {reason}")]
    Probe { path: PathBuf, reason: String },
    #[error("track {0} not found")]
    TrackNotFound(i64),
    #[error("invalid stored value in column {column}: {value}")]
    Corrupt { column: &'static str, value: String },
    #[cfg(feature = "musicbrainz")]
    #[error("musicbrainz: {0}")]
    MusicBrainz(String),
    #[cfg(feature = "musicbrainz")]
    #[error("acoustid: {0}")]
    AcoustId(String),
}

pub type Result<T> = std::result::Result<T, LibraryError>;
