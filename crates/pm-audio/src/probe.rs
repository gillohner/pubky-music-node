//! Cheap file inspection: container, format and duration, no tags and no
//! audio decoded beyond the headers.

use crate::decode::Decoder;
use crate::dsd::DsdReader;
use crate::error::AudioError;
use pm_model::AudioFormat;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    /// `flac`, `alac`, `wav`, `aiff`, `dsf`, `dff`.
    pub container: String,
    pub format: AudioFormat,
    pub duration_ms: Option<u64>,
}

/// Extensions the node scans for.
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "flac", "m4a", "mp4", "alac", "wav", "wave", "aif", "aiff", "dsf", "dff",
];

pub fn is_supported_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn probe_file(path: impl AsRef<Path>) -> Result<FileInfo, AudioError> {
    let path = path.as_ref();
    if is_dsd_file(path)? {
        let r = DsdReader::open(path)?;
        return Ok(FileInfo {
            container: r.container().to_string(),
            format: r.format(),
            duration_ms: r.duration_ms(),
        });
    }
    let d = Decoder::open(path)?;
    Ok(FileInfo {
        container: d.container().to_string(),
        format: d.format(),
        duration_ms: d.duration_ms(),
    })
}

/// DSD by magic bytes, falling back to the extension.
pub(crate) fn is_dsd_file(path: &Path) -> Result<bool, AudioError> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|source| AudioError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let mut magic = [0u8; 4];
    if f.read_exact(&mut magic).is_ok() {
        return Ok(&magic == b"DSD " || &magic == b"FRM8");
    }
    Ok(path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "dsf" | "dff")))
}
