use super::{
    app_path, canonical_uri, chars, limits, sanitize_uri, SchemaError, Validatable, REVIEWS_SEGMENT,
};
use crate::ids::{self, validate_hash_id};
use serde::{Deserialize, Serialize};

/// `/pub/pubky.music/reviews/<hash id>` where the hash is of `target_uri`
/// alone, so a user has **one** review per target: writing another replaces
/// the first (same path), which is the intended "edit my review" behaviour.
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

    pub const PATH_SEGMENT: &'static str = REVIEWS_SEGMENT;

    pub fn create_path(id: &str) -> String {
        format!("{}{}{}", app_path(), Self::PATH_SEGMENT, id)
    }

    /// The data the hash ID covers: the target URI only.
    pub fn id_data(&self) -> String {
        self.target_uri.clone()
    }

    pub fn create_id(&self) -> String {
        ids::hash_id(&self.id_data())
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
            validate_hash_id(id, &self.id_data()).map_err(|_| SchemaError::Id {
                expected: self.create_id(),
                found: id.to_string(),
            })?;
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
        if !(limits::REVIEW_TEXT_MIN..=limits::REVIEW_TEXT_MAX).contains(&n) {
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

    #[test]
    fn id_is_hash_of_target_only() {
        let t = MusicUri::recording(Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap());
        let a = Review::new(&t, Some(5), "great");
        let b = Review::new(&t, Some(1), "meh");
        assert_eq!(a.create_id(), b.create_id());
        assert_eq!(a.create_id().len(), 26);
        let id = a.create_id();
        a.validate(Some(&id)).unwrap();
        assert!(matches!(a.validate(Some("WRONG")), Err(SchemaError::Id { .. })));
        assert!(Review::new(&t, Some(6), "x").validate(None).is_err());
        assert!(Review::new(&t, None, "   ").validate(None).is_err());
        let json = serde_json::to_string(&Review::new(&t, None, "t")).unwrap();
        assert!(!json.contains("rating"));
    }
}
