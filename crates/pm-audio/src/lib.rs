//! Decoding, DSD, the DSP pipeline skeleton and playback helpers for the
//! Pubky Music Node (handoff §6).
//!
//! * [`decode`]: symphonia-based decoding of FLAC, ALAC, WAV and AIFF into
//!   [`pm_model::AudioBlock`]s in the crate-wide integer layout.
//! * [`dsd`]: DSF and DFF readers producing native DSD blocks.
//! * [`pipeline`]: format negotiation (§6.3) and the stage chain (§6.2) with
//!   signal-path tracing (§6.1). Bypass is bit-perfect by construction.
//! * [`probe`]: cheap "what is this file" inspection.
//! * [`play`]: a blocking decode → negotiate → sink loop, used by the CLI and
//!   by the M1 byte-identical test.

pub mod convolve;
pub mod decode;
pub mod dsd;
pub mod dsp;
pub mod error;
pub mod fingerprint;
pub mod loudness;
pub mod pipeline;
pub mod play;
pub mod probe;
pub mod resample;
pub mod zone;

pub use convolve::{Convolver, ImpulseResponse};
pub use decode::Decoder;
pub use dsd::DsdReader;
pub use dsp::{BandKind, Crossfeed, CrossfeedConfig, EqBand, Equalizer};
pub use error::AudioError;
pub use pipeline::{ConvolutionConfig, DsdMode, Loudness, PipelineConfig, PipelineError, Plan};
pub use play::{play_file, play_to_sink, PlayReport};
pub use probe::{probe_file, FileInfo};
pub use zone::LocalFiles;
