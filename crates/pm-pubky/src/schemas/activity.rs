use super::{canonical_uri, chars, limits, sanitize_uri, SchemaError, Validatable};
use crate::ids::{self, validate_timestamp_id};
use serde::{Deserialize, Serialize};

/// Where a play came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ActivitySource {
    /// The node's own library.
    Local,
    /// A streaming service, by its lowercase label (`spotify`, `tidal`, …).
    Service { name: String },
    /// Scrobbled from Roon through the Extension API (handoff §7.4).
    Roon,
    #[default]
    Unknown,
}

/// The `pubky.music/activity` envelope: one coarse "played this" record.
/// Opt-in (handoff §8.3); private by default, a full listening history
/// stays on the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub uri: String,
    /// Microseconds. The node's newer listens have their own id, the time they were written
    /// (pm-api `listens::record`); older ones used this.
    pub played_at: i64,
    #[serde(default)]
    pub source_kind: ActivitySource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl Activity {
    pub fn new(uri: &pm_model::MusicUri, source_kind: ActivitySource, duration_ms: Option<u64>) -> Self {
        Self {
            uri: uri.to_url(),
            played_at: ids::now_micros(),
            source_kind,
            duration_ms,
        }
        .sanitize()
    }

    pub fn create_id(&self) -> String {
        ids::timestamp_id(self.played_at)
    }
}

impl Validatable for Activity {
    fn sanitize(self) -> Self {
        let source_kind = match self.source_kind {
            ActivitySource::Service { name } => ActivitySource::Service {
                name: name.trim().to_lowercase(),
            },
            other => other,
        };
        Self {
            uri: sanitize_uri(&self.uri),
            played_at: self.played_at,
            source_kind,
            duration_ms: self.duration_ms,
        }
    }

    fn validate(&self, id: Option<&str>) -> Result<(), SchemaError> {
        if let Some(id) = id {
            validate_timestamp_id(id).map_err(SchemaError::Validation)?;
        }
        canonical_uri(&self.uri)?;
        if self.played_at <= 0 {
            return Err(SchemaError::validation("played_at must be positive microseconds"));
        }
        if let ActivitySource::Service { name } = &self.source_kind {
            if name.is_empty()
                || chars(name) > limits::SERVICE_NAME_MAX
                || name.chars().any(char::is_whitespace)
            {
                return Err(SchemaError::validation(
                    "service name must be 1–50 characters without whitespace",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_model::{Mbid, MusicUri};

    #[test]
    fn roundtrip_and_source_json() {
        let t = MusicUri::recording(Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap());
        let a = Activity::new(
            &t,
            ActivitySource::Service {
                name: " Tidal ".into(),
            },
            Some(1000),
        );
        assert_eq!(a.source_kind, ActivitySource::Service { name: "tidal".into() });
        let id = a.create_id();
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains(r#""source_kind":{"kind":"service","name":"tidal"}"#));
        let back = <Activity as Validatable>::try_from(json.as_bytes(), &id).unwrap();
        assert_eq!(back, a);
        let minimal: Activity = serde_json::from_str(r#"{"uri":"https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69","played_at":1}"#).unwrap();
        assert_eq!(minimal.source_kind, ActivitySource::Unknown);
        let bad = Activity::new(
            &t,
            ActivitySource::Service {
                name: "two words".into(),
            },
            None,
        );
        assert!(bad.validate(None).is_err());
    }
}
