use super::post::{MusicContent, MusicPost, Visibility};
use super::{Activity, ActivitySource, Playlist, PlaylistItem, Review, SchemaError, PROTOCOL};
use pm_model::MusicUri;

/// Where an object was written: its id, session-relative path, full URL and
/// visibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub id: String,
    pub path: String,
    pub url: String,
    pub visibility: Visibility,
}

/// Mirrors `PubkySpecsBuilder`: takes the author's raw 52-char z32 id and
/// returns validated posts with their write location. Builders do not
/// write; see [`crate::homeserver`].
#[derive(Debug, Clone)]
pub struct MusicSpecsBuilder {
    z32: String,
    visibility: Visibility,
}

impl MusicSpecsBuilder {
    /// `z32` must be the raw 52-character z-base-32 key, never the
    /// `pubky<z32>` display form. Objects are private until
    /// [`with_visibility`](Self::with_visibility) says otherwise.
    pub fn new(z32: &str) -> Result<Self, SchemaError> {
        let z32 = z32.trim();
        if z32.len() != 52 || base32::decode(base32::Alphabet::Z, z32).is_none() {
            return Err(SchemaError::validation(
                "the string is not a 52 character z-base-32 public key",
            ));
        }
        Ok(Self {
            z32: z32.to_string(),
            visibility: Visibility::Private,
        })
    }

    pub fn with_visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }

    pub fn z32(&self) -> &str {
        &self.z32
    }

    pub fn visibility(&self) -> Visibility {
        self.visibility
    }

    fn meta(&self, id: String) -> Meta {
        let path = MusicPost::path(&id, self.visibility);
        let url = format!("{PROTOCOL}{}{path}", self.z32);
        Meta {
            id,
            path,
            url,
            visibility: self.visibility,
        }
    }

    fn finish(&self, content: MusicContent) -> Result<(MusicPost, Meta), SchemaError> {
        let post = MusicPost::new(content);
        let id = post.create_id();
        post.validate(Some(&id))?;
        Ok((post, self.meta(id)))
    }

    pub fn create_playlist(
        &self,
        name: &str,
        description: Option<String>,
        image: Option<String>,
        items: Vec<PlaylistItem>,
    ) -> Result<(MusicPost, Meta), SchemaError> {
        self.finish(MusicContent::Playlist(Playlist::new(
            name,
            description,
            image,
            items,
        )))
    }

    /// Re-validate an edited playlist; the id and path do not change.
    pub fn edit_playlist(&self, mut playlist: Playlist, id: &str) -> Result<(MusicPost, Meta), SchemaError> {
        playlist.touch();
        let post = MusicPost::new(MusicContent::Playlist(playlist));
        if post.create_id() != id {
            return Err(SchemaError::Id {
                expected: post.create_id(),
                found: id.to_string(),
            });
        }
        post.validate(Some(id))?;
        Ok((post, self.meta(id.to_string())))
    }

    pub fn create_review(
        &self,
        target: &MusicUri,
        rating: Option<u8>,
        text: &str,
    ) -> Result<(MusicPost, Meta), SchemaError> {
        self.finish(MusicContent::Review(Review::new(target, rating, text)))
    }

    pub fn create_activity(
        &self,
        uri: &MusicUri,
        source: ActivitySource,
        duration_ms: Option<u64>,
    ) -> Result<(MusicPost, Meta), SchemaError> {
        self.finish(MusicContent::Activity(Activity::new(uri, source, duration_ms)))
    }

    /// A tag on a canonical music URL: a `PubkyAppTag` byte-identical to what
    /// pubky.app would write, under the node's own namespace so Nexus indexes
    /// it as a universal tag: `/pub/pubky.music/tags/<Blake3 of "{uri}:{label}">`.
    /// Tags are always public: a tag on a private playlist makes no sense.
    pub fn create_music_tag(
        &self,
        target: &MusicUri,
        label: &str,
    ) -> Result<(pubky_app_specs::PubkyAppTag, Meta), SchemaError> {
        use pubky_app_specs::traits::{HashId, Validatable as _};
        let tag = pubky_app_specs::PubkyAppTag::new(target.to_url(), label.to_string());
        let id = tag.create_id();
        tag.validate(Some(&id)).map_err(SchemaError::Validation)?;
        let path = super::music_tag_path(&id);
        let url = format!("{PROTOCOL}{}{path}", self.z32);
        Ok((
            tag,
            Meta {
                id,
                path,
                url,
                visibility: Visibility::Public,
            },
        ))
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
    fn playlist_meta_is_private_by_default() {
        let b = MusicSpecsBuilder::new(Z32).unwrap();
        let (post, meta) = b
            .create_playlist("Mix", None, None, vec![PlaylistItem::new(&rec())])
            .unwrap();
        assert_eq!(meta.visibility, Visibility::Private);
        assert_eq!(meta.path, format!("/priv/pubky.app/posts/{}", meta.id));
        assert_eq!(meta.url, format!("pubky://{Z32}{}", meta.path));
        let p = post.as_playlist().unwrap().clone();
        let (edited, meta2) = b.edit_playlist(p.clone(), &meta.id).unwrap();
        assert_eq!(meta2.id, meta.id);
        assert!(edited.as_playlist().unwrap().updated_at >= p.updated_at);
        assert!(b.edit_playlist(p, "0000000000000").is_err());

        let public = b.with_visibility(Visibility::Public);
        let (_, meta) = public.create_review(&rec(), Some(4), "fine").unwrap();
        assert_eq!(meta.path, format!("/pub/pubky.app/posts/{}", meta.id));
    }

    #[test]
    fn tags_are_public_and_spec_shaped() {
        let b = MusicSpecsBuilder::new(Z32).unwrap();
        let (tag, meta) = b.create_music_tag(&rec(), "ambient").unwrap();
        assert_eq!(tag.uri, rec().to_url());
        assert_eq!(meta.visibility, Visibility::Public);
        assert!(
            meta.path.starts_with("/pub/pubky.music/tags/"),
            "Nexus indexes external-link tags only in an app's own namespace"
        );
    }
}
