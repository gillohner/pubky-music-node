use super::{canonical_uri, chars, limits, sanitize_uri, validate_image_url, SchemaError, Validatable};
use crate::ids::{self, validate_timestamp_id};
use serde::{Deserialize, Serialize};

/// One entry of a playlist: a canonical music URL and when it was added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistItem {
    pub uri: String,
    /// Microseconds.
    pub added_at: i64,
}

impl PlaylistItem {
    pub fn new(uri: &pm_model::MusicUri) -> Self {
        Self {
            uri: uri.to_url(),
            added_at: ids::now_micros(),
        }
    }
}

/// The `pubky.music/playlist` envelope of a post (see [`super::post`]).
/// Private by default (`/priv/pubky.app/posts/<timestamp id>`); a public
/// one is an ordinary pubky.app post, so nothing private goes there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default)]
    pub items: Vec<PlaylistItem>,
    /// Microseconds.
    pub created_at: i64,
    /// Microseconds; bumped on every edit.
    pub updated_at: i64,
}

impl Playlist {
    pub fn new(
        name: impl Into<String>,
        description: Option<String>,
        image: Option<String>,
        items: Vec<PlaylistItem>,
    ) -> Self {
        let now = ids::now_micros();
        Self {
            name: name.into(),
            description,
            image,
            items,
            created_at: now,
            updated_at: now,
        }
        .sanitize()
    }

    /// New timestamp ID; use the same ID on later edits.
    pub fn create_id(&self) -> String {
        ids::timestamp_id(self.created_at)
    }

    /// Mark edited: bump `updated_at` (the ID and path stay the same).
    pub fn touch(&mut self) {
        self.updated_at = ids::now_micros();
    }
}

impl Validatable for Playlist {
    fn sanitize(self) -> Self {
        Self {
            name: self.name.trim().to_string(),
            description: self
                .description
                .map(|d| d.trim().to_string())
                .filter(|d| !d.is_empty()),
            image: self.image.map(|i| i.trim().to_string()).filter(|i| !i.is_empty()),
            items: self
                .items
                .into_iter()
                .map(|i| PlaylistItem {
                    uri: sanitize_uri(&i.uri),
                    added_at: i.added_at,
                })
                .collect(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    fn validate(&self, id: Option<&str>) -> Result<(), SchemaError> {
        if let Some(id) = id {
            validate_timestamp_id(id).map_err(SchemaError::Validation)?;
        }
        let n = chars(&self.name);
        if !(limits::PLAYLIST_NAME_MIN..=limits::PLAYLIST_NAME_MAX).contains(&n) {
            return Err(SchemaError::validation(format!(
                "playlist name must be {}–{} characters",
                limits::PLAYLIST_NAME_MIN,
                limits::PLAYLIST_NAME_MAX
            )));
        }
        if let Some(d) = &self.description {
            if chars(d) > limits::PLAYLIST_DESCRIPTION_MAX {
                return Err(SchemaError::validation(format!(
                    "description exceeds {} characters",
                    limits::PLAYLIST_DESCRIPTION_MAX
                )));
            }
        }
        if let Some(i) = &self.image {
            validate_image_url(i, limits::PLAYLIST_IMAGE_URL_MAX)?;
        }
        if self.items.len() > limits::PLAYLIST_ITEMS_MAX {
            return Err(SchemaError::validation(format!(
                "playlist exceeds {} items",
                limits::PLAYLIST_ITEMS_MAX
            )));
        }
        for item in &self.items {
            canonical_uri(&item.uri)?;
            if item.added_at <= 0 {
                return Err(SchemaError::validation(
                    "item added_at must be positive microseconds",
                ));
            }
        }
        if self.created_at <= 0 || self.updated_at < self.created_at {
            return Err(SchemaError::validation(
                "updated_at must be at or after created_at",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_model::{Mbid, MusicUri};

    fn rec() -> MusicUri {
        MusicUri::recording(Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap())
    }

    #[test]
    fn create_validate_roundtrip() {
        let p = Playlist::new(
            "  Late night  ",
            Some("  ".into()),
            None,
            vec![PlaylistItem::new(&rec())],
        );
        assert_eq!(p.name, "Late night");
        assert_eq!(p.description, None);
        let id = p.create_id();
        p.validate(Some(&id)).unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            !json.contains("description"),
            "None fields must be omitted: {json}"
        );
        let back = <Playlist as Validatable>::try_from(json.as_bytes(), &id).unwrap();
        assert_eq!(back, p);
        assert_eq!(
            super::super::post::MusicPost::path(&id, super::super::post::Visibility::Private),
            format!("/priv/pubky.app/posts/{id}")
        );
    }

    #[test]
    fn rejects_bad_items_and_names() {
        let mut p = Playlist::new("x", None, None, vec![]);
        p.items.push(PlaylistItem {
            uri: "music:recording:mbid:abc".into(),
            added_at: 1,
        });
        assert!(matches!(p.validate(None), Err(SchemaError::Uri { .. })));
        let p = Playlist::new("", None, None, vec![]);
        assert!(p.validate(None).is_err());
        let p = Playlist::new("🔥".repeat(100), None, None, vec![]);
        assert!(
            p.validate(None).is_ok(),
            "100 scalar values is the max, not bytes"
        );
        let p = Playlist::new("a".repeat(101), None, None, vec![]);
        assert!(p.validate(None).is_err());
        let p = Playlist::new("ok", None, Some("ftp://x/y.png".into()), vec![]);
        assert!(p.validate(None).is_err());
    }

    #[test]
    fn service_url_items_are_normalized() {
        let p = Playlist::new(
            "x",
            None,
            None,
            vec![PlaylistItem {
                uri: " https://open.spotify.com/intl-de/track/4cOdK2wGLETKBW3PvgPWqT?si=1 ".into(),
                added_at: 5,
            }],
        );
        assert_eq!(
            p.items[0].uri,
            "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT"
        );
        p.validate(None).unwrap();
    }

    #[test]
    fn null_optional_fields_are_accepted() {
        let json = r#"{"name":"n","description":null,"image":null,"items":[],"created_at":1,"updated_at":1}"#;
        let p: Playlist = serde_json::from_str(json).unwrap();
        assert_eq!(p.description, None);
    }
}
