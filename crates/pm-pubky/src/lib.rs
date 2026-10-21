//! Pubky integration for the music node (handoff §8 and §12).
//!
//! Decisions recorded here, not re-litigated elsewhere:
//!
//! * **Canonical URIs are URLs**: MusicBrainz recording/release/release-group/
//!   artist URLs, or the service's own canonical URL when no MBID is known
//!   (`pm_model::MusicUri`). The `music:` scheme proposed in handoff §8.2 is
//!   retired because Pinakes refuses unknown schemes (§12).
//! * **App namespace**: `pubky.music`, as the single constant
//!   [`schemas::APP_NAMESPACE`]. The final name is open (handoff §11); change it
//!   in one place.
//! * **Tags reuse the pubky.app object**: a music tag is a `PubkyAppTag` under
//!   the node's own namespace, `/pub/pubky.music/tags/<id>`, whose `uri` is the
//!   canonical URL. Nexus indexes tags on external links only as universal tags
//!   in an app's own namespace (under `pubky.app/tags/` they must target
//!   `pubky://` objects and are dropped), so Nexus and Pinakes see them.
//! * **IDs follow pubky-app-specs**: timestamp IDs (microseconds → 8 big-endian
//!   bytes → Crockford base32, 13 chars) and hash IDs (Blake3, first 16 bytes →
//!   Crockford base32, 26 chars). Every `created_at` is microseconds.
//!
//! Modules: [`schemas`] (the social objects), [`homeserver`] (read/write them
//! through the `pubky` SDK), [`pinakes`] (read-only cards and search),
//! [`nexus`] (who tagged what), [`discovery`] (the §8.4 queries composed over
//! the two).

pub mod config;
pub mod discovery;
pub mod homeserver;
pub mod ids;
pub mod nexus;
pub mod pinakes;
pub mod schemas;
pub mod v1;

pub use config::{Network, PmPubkyConfig};
pub use discovery::{DiscoveredItem, Discovery};
pub use nexus::{NexusClient, NexusError};
pub use pinakes::{Card, PinakesClient, PinakesError, SearchQuery, SearchResponse};
pub use schemas::{
    Activity, ActivitySource, Favourite, Meta, MusicContent, MusicPost, MusicSpecsBuilder, Playlist,
    PlaylistItem, Review, SchemaError, Visibility, APP_NAMESPACE,
};
