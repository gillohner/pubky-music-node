use super::{Activity, ActivitySource, Playlist, PlaylistItem, Review, SchemaError, Validatable, PROTOCOL};
use pm_model::MusicUri;

/// Where an object was written: its id, session-relative path and full URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub id: String,
    pub path: String,
    pub url: String,
}

/// Mirrors `PubkySpecsBuilder`: takes the author's raw 52-char z32 id and
/// returns validated objects with their write location. Builders do not
/// write; see [`crate::homeserver`].
#[derive(Debug, Clone)]
pub struct MusicSpecsBuilder {
    z32: String,
}

impl MusicSpecsBuilder {
    /// `z32` must be the raw 52-character z-base-32 key, never the
    /// `pubky<z32>` display form.
    pub fn new(z32: &str) -> Result<Self, SchemaError> {
        let z32 = z32.trim();
        if z32.len() != 52 || base32::decode(base32::Alphabet::Z, z32).is_none() {
            return Err(SchemaError::validation(
                "the string is not a 52 character z-base-32 public key",
            ));
        }
        Ok(Self { z32: z32.to_string() })
    }

    pub fn z32(&self) -> &str {
        &self.z32
    }

    fn meta(&self, id: String, path: String) -> Meta {
        let url = format!("{PROTOCOL}{}{path}", self.z32);
        Meta { id, path, url }
    }

    pub fn create_playlist(
        &self,
        name: &str,
        description: Option<String>,
        image: Option<String>,
        items: Vec<PlaylistItem>,
    ) -> Result<(Playlist, Meta), SchemaError> {
        let playlist = Playlist::new(name, description, image, items);
        let id = playlist.create_id();
        playlist.validate(Some(&id))?;
        let path = Playlist::create_path(&id);
        Ok((playlist, self.meta(id, path)))
    }

    /// Re-validate an edited playlist; the id and path do not change.
    pub fn edit_playlist(&self, mut playlist: Playlist, id: &str) -> Result<(Playlist, Meta), SchemaError> {
        playlist.touch();
        let playlist = playlist.sanitize();
        playlist.validate(Some(id))?;
        let path = Playlist::create_path(id);
        Ok((playlist, self.meta(id.to_string(), path)))
    }

    pub fn create_review(
        &self,
        target: &MusicUri,
        rating: Option<u8>,
        text: &str,
    ) -> Result<(Review, Meta), SchemaError> {
        let review = Review::new(target, rating, text);
        let id = review.create_id();
        review.validate(Some(&id))?;
        let path = Review::create_path(&id);
        Ok((review, self.meta(id, path)))
    }

    pub fn create_activity(
        &self,
        uri: &MusicUri,
        source: ActivitySource,
        duration_ms: Option<u64>,
    ) -> Result<(Activity, Meta), SchemaError> {
        let activity = Activity::new(uri, source, duration_ms);
        let id = activity.create_id();
        activity.validate(Some(&id))?;
        let path = Activity::create_path(&id);
        Ok((activity, self.meta(id, path)))
    }

    /// A pubky.app tag on a canonical music URL, byte-identical to what
    /// pubky.app would write: `/pub/pubky.app/tags/<Blake3 of "{uri}:{label}">`.
    #[cfg(feature = "app-specs")]
    pub fn create_music_tag(
        &self,
        target: &MusicUri,
        label: &str,
    ) -> Result<(pubky_app_specs::PubkyAppTag, Meta), SchemaError> {
        use pubky_app_specs::traits::{HasIdPath, HashId, Validatable as _};
        let tag = pubky_app_specs::PubkyAppTag::new(target.to_url(), label.to_string());
        let id = tag.create_id();
        tag.validate(Some(&id)).map_err(SchemaError::Validation)?;
        let path = pubky_app_specs::PubkyAppTag::create_path(&id);
        Ok((tag, self.meta(id, path)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_model::Mbid;

    const Z32: &str = "8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo";

    fn rec() -> MusicUri {
        MusicUri::recording(Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap())
    }

    #[test]
    fn rejects_display_form() {
        assert!(MusicSpecsBuilder::new(&format!("pubky{Z32}")).is_err());
        assert!(MusicSpecsBuilder::new(Z32).is_ok());
    }

    #[test]
    fn playlist_meta() {
        let b = MusicSpecsBuilder::new(Z32).unwrap();
        let (p, meta) = b
            .create_playlist("Mix", None, None, vec![PlaylistItem::new(&rec())])
            .unwrap();
        assert_eq!(meta.path, format!("/pub/pubky.music/playlists/{}", meta.id));
        assert_eq!(meta.url, format!("pubky://{Z32}{}", meta.path));
        let (edited, meta2) = b.edit_playlist(p.clone(), &meta.id).unwrap();
        assert_eq!(meta2.id, meta.id);
        assert!(edited.updated_at >= p.updated_at);
        assert!(b.create_playlist("", None, None, vec![]).is_err());
    }

    #[test]
    fn review_and_activity_meta() {
        let b = MusicSpecsBuilder::new(Z32).unwrap();
        let (_, m) = b.create_review(&rec(), Some(4), "nice").unwrap();
        assert!(m.path.starts_with("/pub/pubky.music/reviews/"));
        assert_eq!(m.id.len(), 26);
        let (_, m) = b.create_activity(&rec(), ActivitySource::Local, None).unwrap();
        assert!(m.path.starts_with("/pub/pubky.music/activity/"));
        assert_eq!(m.id.len(), 13);
    }

    #[cfg(feature = "app-specs")]
    #[test]
    fn music_tag_matches_app_specs_hash() {
        let b = MusicSpecsBuilder::new(Z32).unwrap();
        let (tag, meta) = b.create_music_tag(&rec(), " Favourite ").unwrap();
        assert_eq!(tag.label, "favourite");
        assert_eq!(tag.uri, rec().to_url());
        let expected = crate::ids::hash_id(&format!("{}:{}", tag.uri, tag.label));
        assert_eq!(meta.id, expected);
        assert_eq!(meta.path, format!("/pub/pubky.app/tags/{expected}"));
        assert!(b.create_music_tag(&rec(), "two words").is_err());
    }
}
