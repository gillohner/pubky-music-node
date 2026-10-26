//! The music kind vocabulary and how each kind's content combines with the envelope.

use std::fmt;

use pubky_social_specs::{
    AllowedSchemes, ContentReference, PostEnvelope, PostKind, ValidationCtx, VALIDATION_LIMITS,
};
use serde::{Deserialize, Serialize};

use crate::content;

/// What a music post is. The wire spelling is lowercase (`"review"`, `"listen"`); a spelling this
/// version does not know reads as [`MusicPostKind::Unknown`] and fails validation.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
#[serde(from = "String", into = "String")]
#[non_exhaustive]
pub enum MusicPostKind {
    /// A rating of 1 to 10 with optional text; `embed` is the reviewed music URL.
    #[default]
    Review,
    /// One play of the music at `embed`, under `/priv/` (a private history) or `/pub/` (a public
    /// one); the person chooses.
    Listen,
    /// A kind a newer writer used, kept with its spelling.
    Unknown(String),
}

impl MusicPostKind {
    /// The wire spelling.
    pub fn wire_name(&self) -> &str {
        match self {
            MusicPostKind::Review => "review",
            MusicPostKind::Listen => "listen",
            MusicPostKind::Unknown(name) => name,
        }
    }
}

impl From<String> for MusicPostKind {
    fn from(s: String) -> Self {
        match s.as_str() {
            "review" => MusicPostKind::Review,
            "listen" => MusicPostKind::Listen,
            _ => MusicPostKind::Unknown(s),
        }
    }
}

impl From<MusicPostKind> for String {
    fn from(k: MusicPostKind) -> Self {
        k.wire_name().to_string()
    }
}

impl fmt::Display for MusicPostKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_name())
    }
}

impl PostKind for MusicPostKind {
    fn is_known(&self) -> bool {
        !matches!(self, MusicPostKind::Unknown(_))
    }

    fn validate_content(&self, post: &PostEnvelope<Self>, _ctx: &ValidationCtx) -> Result<(), String> {
        // Both kinds are about one piece of music, named by `embed`. A reply to a review is a
        // social post with `parent` set to the review, not another review.
        if post.embed.is_none() {
            return Err(format!(
                "Validation Error: a {self} must embed the music URL it is about"
            ));
        }
        if post.parent.is_some() {
            return Err(format!("Validation Error: a {self} cannot have a parent"));
        }
        match self {
            MusicPostKind::Review => content::parse_review(&post.content).map(|_| ()),
            MusicPostKind::Listen => {
                // Either root: the person picks a private or a public history. A public listen's
                // source passes the reference gate under the public root, so it never points
                // at a private object.
                if !post.attachments.is_empty() || post.lock.is_some() {
                    return Err("Validation Error: a listen has no attachments and no lock".into());
                }
                content::parse_listen(&post.content).map(|_| ())
            }
            // The envelope refuses Unknown before it asks the kind
            MusicPostKind::Unknown(name) => Err(format!("Validation Error: unknown music kind {name}")),
        }
    }

    /// A listen's `source.uri`: http(s) or pubky only, checked by the envelope's gate with the
    /// author in scope.
    fn content_references(&self, post: &PostEnvelope<Self>) -> Vec<ContentReference> {
        if *self != MusicPostKind::Listen {
            return Vec::new();
        }
        content::listen_source_uri(&post.content)
            .map(|uri| ContentReference {
                field: "content.source.uri".into(),
                uri,
                schemes: AllowedSchemes::PubkyHttpHttps,
                max_code_points: VALIDATION_LIMITS.reference_uri_max_length,
            })
            .into_iter()
            .collect()
    }
}
