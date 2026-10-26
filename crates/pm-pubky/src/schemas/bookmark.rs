//! Bookmarks (once "favourites" or "hearts") of canonical music URLs (`pm_model::MusicUri`). The
//! node's id for one is the pubky-app-specs `PubkyAppBookmark` hash of the URI. They are written
//! as social v1 bookmarks (`/priv/social/v1/bookmarks/`, see [`crate::v1`]); the older
//! `/priv/pubky.app/bookmarks/<id>` form below is read once for the move to v1 (ADR 0006, 0014).

use super::Visibility;
use pm_model::MusicUri;
use pubky_app_specs::traits::HashId;
use pubky_app_specs::PubkyAppBookmark;

pub const BOOKMARKS_DIR: &str = "pubky.app/bookmarks/";

/// A bookmark as the node stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub id: String,
    pub uri: MusicUri,
    /// Microseconds.
    pub created_at: i64,
}

impl Bookmark {
    pub fn new(uri: MusicUri) -> Self {
        let b = PubkyAppBookmark::new(uri.to_url());
        Self {
            id: b.create_id(),
            uri,
            created_at: b.created_at,
        }
    }

    /// The id the spec derives from a URI.
    pub fn id_for(uri: &MusicUri) -> String {
        PubkyAppBookmark {
            uri: uri.to_url(),
            created_at: 0,
        }
        .create_id()
    }

    pub fn to_bookmark(&self) -> PubkyAppBookmark {
        PubkyAppBookmark {
            uri: self.uri.to_url(),
            created_at: self.created_at,
        }
    }

    /// Parse a bookmark body; `None` when it is not a music URL (a regular
    /// pubky.app bookmark of a post, say).
    pub fn from_bookmark(id: &str, b: &PubkyAppBookmark) -> Option<Self> {
        let uri = MusicUri::parse(&b.uri).ok()?;
        Some(Self {
            id: id.to_string(),
            uri,
            created_at: b.created_at,
        })
    }

    /// `/priv/pubky.app/bookmarks/<id>` or the public equivalent.
    pub fn path(id: &str, visibility: Visibility) -> String {
        format!("{}{BOOKMARKS_DIR}{id}", visibility.prefix())
    }

    pub fn dir(visibility: Visibility) -> String {
        format!("{}{BOOKMARKS_DIR}", visibility.prefix())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_model::Mbid;
    use pubky_app_specs::traits::HasIdPath;

    #[test]
    fn ids_follow_the_spec_and_paths_mirror_posts() {
        let mbid = Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap();
        let f = Bookmark::new(MusicUri::Recording { mbid });
        assert_eq!(f.id, Bookmark::id_for(&MusicUri::Recording { mbid }));
        assert_eq!(
            f.id,
            f.to_bookmark().create_id(),
            "id is a pure function of the URI"
        );
        assert_eq!(
            Bookmark::path(&f.id, Visibility::Private),
            format!("/priv/pubky.app/bookmarks/{}", f.id)
        );
        assert_eq!(
            Bookmark::path(&f.id, Visibility::Public),
            pubky_app_specs::PubkyAppBookmark::create_path(&f.id)
        );
        let back = Bookmark::from_bookmark(&f.id, &f.to_bookmark()).unwrap();
        assert_eq!(back, f);
        let other = PubkyAppBookmark::new("pubky://x/pub/pubky.app/posts/1".into());
        assert!(Bookmark::from_bookmark("y", &other).is_none());
    }
}
