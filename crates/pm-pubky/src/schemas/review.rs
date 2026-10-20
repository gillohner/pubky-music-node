use super::{canonical_uri, chars, limits, sanitize_uri, SchemaError, Validatable};
use crate::ids::{self, validate_timestamp_id};
use serde::{Deserialize, Serialize};

/// The `pubky.music/review` envelope of a post. Posts carry Timestamp IDs,
/// so "one review per target" is a reader rule: the newest review of a
/// target by an author wins.
///
/// `rating` is 1–5 (stars). It is optional so a text-only review is valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub target_uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    pub text: String,
    /// Microseconds.
    pub created_at: i64,
}

impl Review {
    pub fn new(target: &pm_model::MusicUri, rating: Option<u8>, text: impl Into<String>) -> Self {
        Self {
            target_uri: target.to_url(),
            rating,
            text: text.into(),
            created_at: ids::now_micros(),
        }
        .sanitize()
    }

    pub fn create_id(&self) -> String {
        ids::timestamp_id(self.created_at)
    }
}

impl Validatable for Review {
    fn sanitize(self) -> Self {
        Self {
            target_uri: sanitize_uri(&self.target_uri),
            rating: self.rating,
            text: self.text.trim().to_string(),
            created_at: self.created_at,
        }
    }

    fn validate(&self, id: Option<&str>) -> Result<(), SchemaError> {
        if let Some(id) = id {
            validate_timestamp_id(id).map_err(SchemaError::Validation)?;
        }
        canonical_uri(&self.target_uri)?;
        if let Some(r) = self.rating {
            if !(limits::REVIEW_RATING_MIN..=limits::REVIEW_RATING_MAX).contains(&r) {
                return Err(SchemaError::validation(format!(
                    "rating must be {}–{}",
                    limits::REVIEW_RATING_MIN,
                    limits::REVIEW_RATING_MAX
                )));
            }
        }
        let n = chars(&self.text);
        // A rating alone is a valid review; text is required only without one.
        let min = if self.rating.is_some() {
            0
        } else {
            limits::REVIEW_TEXT_MIN
        };
        if !(min..=limits::REVIEW_TEXT_MAX).contains(&n) {
            return Err(SchemaError::validation(format!(
                "review text must be {}–{} characters",
                limits::REVIEW_TEXT_MIN,
                limits::REVIEW_TEXT_MAX
            )));
        }
        if self.created_at <= 0 {
            return Err(SchemaError::validation(
                "created_at must be positive microseconds",
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
    fn id_is_the_creation_timestamp() {
        let r = Review::new(&rec(), Some(5), "Great");
        let id = r.create_id();
        assert_eq!(id, crate::ids::timestamp_id(r.created_at));
        r.validate(Some(&id)).unwrap();
        // Two reviews of the same target are distinct posts; readers keep the newest.
        let later = Review {
            created_at: r.created_at + 1,
            ..r.clone()
        };
        assert_ne!(later.create_id(), id);
        assert!(r.validate(Some("not-an-id")).is_err());
    }
}
