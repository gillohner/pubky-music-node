//! From a file's tags to a canonical URI (handoff §8.2).
//!
//! Resolution order on the node: MBID → ISRC → AcoustID fingerprint → fuzzy
//! match. This module does the first two, which need no network: they come
//! straight from tags. The other two are the job of a [`Resolver`].

use crate::tags::TagSet;
use pm_model::{Confidence, MusicUri};

/// Assign a canonical URI from tags alone.
///
/// * recording MBID → `MusicUri::Recording`, `Confidence::TaggedMbid`;
/// * else ISRC → `MusicUri::Isrc`, `Confidence::TaggedIsrc`;
/// * else `None`.
pub fn assign(tags: &TagSet) -> Option<(MusicUri, Confidence)> {
    if let Some(mbid) = tags.recording_mbid {
        return Some((MusicUri::Recording { mbid }, Confidence::TaggedMbid));
    }
    if let Some(isrc) = tags.isrc.clone() {
        return Some((MusicUri::Isrc { isrc }, Confidence::TaggedIsrc));
    }
    None
}

/// A network- or fingerprint-backed resolver for tracks [`assign`] could not
/// place: AcoustID (fingerprint → recording MBID) and fuzzy matching
/// (artist / title / duration ± 2 s against MusicBrainz search).
///
/// Implementations must report a [`Confidence`] honestly; the caller keeps
/// the result locally and publishes only above
/// [`Confidence::PUBLISH_THRESHOLD`]. Later milestones implement this; the
/// MusicBrainz client in [`crate::musicbrainz`] provides the search half.
pub trait Resolver: Send + Sync {
    fn resolve(
        &self,
        tags: &TagSet,
        duration_ms: Option<u64>,
    ) -> impl std::future::Future<Output = crate::Result<Option<(MusicUri, Confidence)>>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_model::{Isrc, Mbid};

    #[test]
    fn mbid_wins_over_isrc() {
        let mut t = TagSet::default();
        t.isrc = Some(Isrc::parse("USRC17607839").unwrap());
        let (u, c) = assign(&t).unwrap();
        assert_eq!(u.to_url(), "https://musicbrainz.org/isrc/USRC17607839");
        assert_eq!(c, Confidence::TaggedIsrc);
        t.recording_mbid = Some(Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap());
        let (u, c) = assign(&t).unwrap();
        assert!(u.is_canonical());
        assert_eq!(c, Confidence::TaggedMbid);
        assert!(assign(&TagSet::default()).is_none());
    }
}
