//! Shared types for the Pubky Music Node.
//!
//! This crate holds the vocabulary every other crate speaks: audio formats and
//! capabilities (§6.3 of the handoff), the signal-path trace (§6.1), the
//! in-memory audio block that flows from decoder to output, canonical music
//! identity (§8.2 as overridden by §12: MusicBrainz URLs, never a `music:`
//! scheme), and the library entities.
//!
//! It has no I/O and no async; it is safe to depend on from every layer,
//! including the client core over FFI.

pub mod audio;
pub mod block;
pub mod ids;
pub mod library;
pub mod path;

pub use audio::{AudioFormat, DsdSupport, OutputCaps, QualityCeiling, SampleKind, Volume, VolumeKind};
pub use block::{AudioBlock, Samples};
pub use ids::{Isrc, Mbid, MusicEntity, MusicUri, Service, ServiceLink, ServiceLinkKind, Upc};
pub use library::{Album, Artist, AudioFile, Confidence, Track, TrackId};
pub use path::{PathStage, Quality, SignalPath};
