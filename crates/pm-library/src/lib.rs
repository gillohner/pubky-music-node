//! The music library: what is on disk, what it is, and how it maps to
//! canonical identity (handoff §5.1 `pm-library`, milestone M1).
//!
//! * [`db`]: the SQLite catalogue (`Library`).
//! * [`tags`]: container probing and tag reading for FLAC, ALAC/M4A, WAV,
//!   AIFF, DSF and DFF.
//! * [`scan`]: the folder scanner that keeps the catalogue in sync.
//! * [`canonical`]: from tags to a [`pm_model::MusicUri`] with a
//!   [`pm_model::Confidence`].
//! * [`musicbrainz`] (feature `musicbrainz`, default): the MusicBrainz web
//!   service client used for resolution.
//!
//! Everything in [`db`] and [`scan`] is synchronous; the node wraps calls in
//! `spawn_blocking`.

pub mod canonical;
pub mod db;
pub mod dsd;
pub mod error;
pub mod import;
pub mod lyrics;
#[cfg(feature = "musicbrainz")]
pub mod musicbrainz;
pub mod posts;
pub mod scan;
pub mod tags;

pub use canonical::{assign, Resolver};
pub use db::{AlbumArt, ArtistSummary, FavouriteRow, GenreSummary, PlayEntry, PlayedTrack, TagRow};
pub use db::{Library, LibraryStats};
pub use error::{LibraryError, Result};
pub use import::{ImportFormat, ImportedList, ImportedTrack};
pub use lyrics::LyricLine;
pub use posts::MusicPostRow;
pub use scan::{ScanReport, Scanner};
pub use tags::{probe_and_read, FileProbe, Picture, TagSet};
