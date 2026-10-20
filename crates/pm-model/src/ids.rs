//! Canonical music identity.
//!
//! The handoff's §8.2 proposed a `music:` URI scheme. §12 (the Pinakes
//! context) overrides it: Pinakes refuses unknown schemes and wants **real
//! URLs**, so every social object (tag, playlist item, activity) targets:
//!
//! * recording: `https://musicbrainz.org/recording/<mbid>` (preferred);
//! * release: `https://musicbrainz.org/release/<mbid>`, or
//!   `https://musicbrainz.org/release-group/<mbid>` for the album as a work;
//! * artist: `https://musicbrainz.org/artist/<mbid>`;
//! * with no MBID yet: the streaming service's own canonical URL, which
//!   Pinakes links to the MBID on its side.
//!
//! As an ISRC fallback this crate uses `https://musicbrainz.org/isrc/<ISRC>`,
//! a real MusicBrainz page. That choice is recorded in ADR 0001 and still has
//! to be agreed with the Pinakes owner.
//!
//! Nothing here touches the network. Resolution (service URL → MBID) lives in
//! `pm-library` and `pm-pubky`.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use url::Url;
use uuid::Uuid;

/// A MusicBrainz identifier: a UUID, always rendered lowercase hyphenated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Mbid(pub Uuid);

impl Mbid {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        Uuid::parse_str(s.trim())
            .map(Mbid)
            .map_err(|_| IdError::InvalidMbid(s.to_string()))
    }
}

impl fmt::Display for Mbid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.as_hyphenated())
    }
}

impl FromStr for Mbid {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Mbid::parse(s)
    }
}

/// International Standard Recording Code: 12 characters, `CC XXX YY NNNNN`.
/// Stored normalized: uppercase, no hyphens.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Isrc(String);

impl Isrc {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let norm: String = s
            .trim()
            .chars()
            .filter(|c| *c != '-' && *c != ' ')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let b = norm.as_bytes();
        let ok = b.len() == 12
            && b[0..2].iter().all(u8::is_ascii_alphabetic)
            && b[2..5].iter().all(u8::is_ascii_alphanumeric)
            && b[5..12].iter().all(u8::is_ascii_digit);
        if ok {
            Ok(Isrc(norm))
        } else {
            Err(IdError::InvalidIsrc(s.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Isrc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Isrc {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Isrc::parse(s)
    }
}

/// A UPC/EAN barcode: 12 to 14 digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Upc(String);

impl Upc {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let norm: String = s.trim().chars().filter(|c| *c != '-' && *c != ' ').collect();
        if (12..=14).contains(&norm.len()) && norm.bytes().all(|b| b.is_ascii_digit()) {
            Ok(Upc(norm))
        } else {
            Err(IdError::InvalidUpc(s.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Upc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What kind of thing a URI names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MusicEntity {
    Recording,
    Release,
    ReleaseGroup,
    Artist,
}

/// Streaming services Pinakes's seed data covers (§12) plus the ones the
/// handoff names. The `Display` form is the lowercase label used in tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Spotify,
    Tidal,
    Qobuz,
    AppleMusic,
    Deezer,
    YouTube,
    YouTubeMusic,
    AmazonMusic,
    Beatport,
    SoundCloud,
    Bandcamp,
    Audiomack,
    /// archive.org items (Live Music Archive, netlabels, public domain).
    InternetArchive,
    /// audius.co tracks and artists.
    Audius,
}

impl Service {
    pub fn label(&self) -> &'static str {
        match self {
            Service::Spotify => "spotify",
            Service::Tidal => "tidal",
            Service::Qobuz => "qobuz",
            Service::AppleMusic => "apple-music",
            Service::Deezer => "deezer",
            Service::YouTube => "youtube",
            Service::YouTubeMusic => "youtube-music",
            Service::AmazonMusic => "amazon-music",
            Service::Beatport => "beatport",
            Service::SoundCloud => "soundcloud",
            Service::Bandcamp => "bandcamp",
            Service::InternetArchive => "internet-archive",
            Service::Audius => "audius",
            Service::Audiomack => "audiomack",
        }
    }
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Track, album or artist on a service. `Unknown` is a page we recognise as
/// belonging to the service but cannot type (an artist's SoundCloud profile,
/// a Bandcamp label page).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceLinkKind {
    Track,
    Album,
    Artist,
    Unknown,
}

/// A link into a streaming service, normalized to the service's canonical
/// form (tracking parameters dropped, locale prefixes removed where the
/// service ignores them).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServiceLink {
    pub service: Service,
    pub kind: ServiceLinkKind,
    /// The service's own id for the item where the URL carries one.
    pub id: Option<String>,
    /// The canonical URL, the one to tag and to send to Pinakes.
    pub url: String,
}

/// A canonical music URI: the thing tags and playlist items point at.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MusicUri {
    Recording {
        mbid: Mbid,
    },
    Release {
        mbid: Mbid,
    },
    ReleaseGroup {
        mbid: Mbid,
    },
    Artist {
        mbid: Mbid,
    },
    /// Fallback when no MBID is known yet (ADR 0001).
    Isrc {
        isrc: Isrc,
    },
    /// Fallback when neither MBID nor ISRC is known.
    Service(ServiceLink),
}

pub const MUSICBRAINZ_BASE: &str = "https://musicbrainz.org/";

impl MusicUri {
    pub fn recording(mbid: Mbid) -> Self {
        MusicUri::Recording { mbid }
    }

    pub fn entity(&self) -> Option<MusicEntity> {
        match self {
            MusicUri::Recording { .. } | MusicUri::Isrc { .. } => Some(MusicEntity::Recording),
            MusicUri::Release { .. } => Some(MusicEntity::Release),
            MusicUri::ReleaseGroup { .. } => Some(MusicEntity::ReleaseGroup),
            MusicUri::Artist { .. } => Some(MusicEntity::Artist),
            MusicUri::Service(l) => match l.kind {
                ServiceLinkKind::Track => Some(MusicEntity::Recording),
                ServiceLinkKind::Album => Some(MusicEntity::Release),
                ServiceLinkKind::Artist => Some(MusicEntity::Artist),
                ServiceLinkKind::Unknown => None,
            },
        }
    }

    /// True for MBID-backed URIs: the preferred form for publishing.
    pub fn is_canonical(&self) -> bool {
        matches!(
            self,
            MusicUri::Recording { .. }
                | MusicUri::Release { .. }
                | MusicUri::ReleaseGroup { .. }
                | MusicUri::Artist { .. }
        )
    }

    pub fn mbid(&self) -> Option<Mbid> {
        match self {
            MusicUri::Recording { mbid }
            | MusicUri::Release { mbid }
            | MusicUri::ReleaseGroup { mbid }
            | MusicUri::Artist { mbid } => Some(*mbid),
            _ => None,
        }
    }

    /// The URL string to put on the wire.
    pub fn to_url(&self) -> String {
        match self {
            MusicUri::Recording { mbid } => format!("{MUSICBRAINZ_BASE}recording/{mbid}"),
            MusicUri::Release { mbid } => format!("{MUSICBRAINZ_BASE}release/{mbid}"),
            MusicUri::ReleaseGroup { mbid } => format!("{MUSICBRAINZ_BASE}release-group/{mbid}"),
            MusicUri::Artist { mbid } => format!("{MUSICBRAINZ_BASE}artist/{mbid}"),
            MusicUri::Isrc { isrc } => format!("{MUSICBRAINZ_BASE}isrc/{isrc}"),
            MusicUri::Service(l) => l.url.clone(),
        }
    }

    /// Parse any supported URL. Rejects the retired `music:` scheme with a
    /// dedicated error so callers can explain the migration.
    pub fn parse(input: &str) -> Result<Self, IdError> {
        let s = input.trim();
        if s.len() > 2048 {
            return Err(IdError::TooLong);
        }
        if s.to_ascii_lowercase().starts_with("music:") {
            return Err(IdError::MusicSchemeRetired);
        }
        let url = Url::parse(s).map_err(|_| IdError::NotAUrl(s.to_string()))?;
        if url.scheme() != "https" && url.scheme() != "http" {
            return Err(IdError::UnsupportedScheme(url.scheme().to_string()));
        }
        let host = url
            .host_str()
            .ok_or_else(|| IdError::NotAUrl(s.to_string()))?
            .to_ascii_lowercase();
        let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
        let segs: Vec<&str> = url
            .path_segments()
            .map(|p| p.filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();

        if host == "musicbrainz.org" || host == "beta.musicbrainz.org" {
            return match segs.as_slice() {
                ["recording", id, ..] => Ok(MusicUri::Recording {
                    mbid: Mbid::parse(id)?,
                }),
                ["release", id, ..] => Ok(MusicUri::Release {
                    mbid: Mbid::parse(id)?,
                }),
                ["release-group", id, ..] => Ok(MusicUri::ReleaseGroup {
                    mbid: Mbid::parse(id)?,
                }),
                ["artist", id, ..] => Ok(MusicUri::Artist {
                    mbid: Mbid::parse(id)?,
                }),
                ["isrc", code, ..] => Ok(MusicUri::Isrc {
                    isrc: Isrc::parse(code)?,
                }),
                _ => Err(IdError::UnknownMusicBrainzPath(url.path().to_string())),
            };
        }

        service::parse(&url, &host, &segs)
            .map(MusicUri::Service)
            .ok_or(IdError::UnknownHost(host))
    }
}

impl fmt::Display for MusicUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_url())
    }
}

impl FromStr for MusicUri {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        MusicUri::parse(s)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdError {
    #[error("invalid MBID: {0}")]
    InvalidMbid(String),
    #[error("invalid ISRC: {0}")]
    InvalidIsrc(String),
    #[error("invalid UPC/EAN: {0}")]
    InvalidUpc(String),
    #[error("the music: scheme is retired; use a MusicBrainz or service URL (ADR 0001)")]
    MusicSchemeRetired,
    #[error("not a URL: {0}")]
    NotAUrl(String),
    #[error("unsupported URL scheme: {0}")]
    UnsupportedScheme(String),
    #[error("unknown MusicBrainz path: {0}")]
    UnknownMusicBrainzPath(String),
    #[error("not a known music service host: {0}")]
    UnknownHost(String),
    #[error("URI longer than 2048 bytes")]
    TooLong,
}

/// Service URL recognition. One function per service, each returning the
/// canonical form. Kept deliberately conservative: a URL we cannot type
/// becomes `Unknown` rather than a wrong `Track`.
mod service {
    use super::{Service, ServiceLink, ServiceLinkKind};
    use url::Url;

    pub fn parse(url: &Url, host: &str, segs: &[&str]) -> Option<ServiceLink> {
        match host {
            "open.spotify.com" | "play.spotify.com" => spotify(segs),
            "tidal.com" | "listen.tidal.com" => tidal(segs),
            "open.qobuz.com" | "qobuz.com" | "play.qobuz.com" => qobuz(host, segs),
            "music.apple.com" | "itunes.apple.com" => apple(url, segs),
            "deezer.com" => deezer(segs),
            "youtube.com" | "m.youtube.com" => youtube(url, segs, Service::YouTube),
            "youtu.be" => youtu_be(segs),
            "music.youtube.com" => youtube(url, segs, Service::YouTubeMusic),
            "beatport.com" => beatport(segs),
            "soundcloud.com" | "m.soundcloud.com" | "on.soundcloud.com" => soundcloud(segs),
            "audiomack.com" => audiomack(segs),
            "archive.org" | "www.archive.org" => archive(segs),
            "audius.co" | "www.audius.co" => audius(segs),
            h if h.starts_with("music.amazon.") => amazon(url, host, segs),
            h if h.ends_with(".bandcamp.com") => bandcamp(host, segs),
            _ => None,
        }
    }

    fn link(service: Service, kind: ServiceLinkKind, id: Option<&str>, url: String) -> Option<ServiceLink> {
        Some(ServiceLink {
            service,
            kind,
            id: id.map(str::to_string),
            url,
        })
    }

    fn spotify(segs: &[&str]) -> Option<ServiceLink> {
        // Locale prefix "intl-de" etc. is ignored by Spotify.
        let segs: Vec<&str> = segs.iter().copied().filter(|s| !s.starts_with("intl-")).collect();
        let (kind, id) = match segs.as_slice() {
            ["track", id, ..] => (ServiceLinkKind::Track, *id),
            ["album", id, ..] => (ServiceLinkKind::Album, *id),
            ["artist", id, ..] => (ServiceLinkKind::Artist, *id),
            _ => return None,
        };
        let seg = match kind {
            ServiceLinkKind::Track => "track",
            ServiceLinkKind::Album => "album",
            _ => "artist",
        };
        link(
            Service::Spotify,
            kind,
            Some(id),
            format!("https://open.spotify.com/{seg}/{id}"),
        )
    }

    fn tidal(segs: &[&str]) -> Option<ServiceLink> {
        let segs: Vec<&str> = segs.iter().copied().filter(|s| *s != "browse").collect();
        let (kind, id) = match segs.as_slice() {
            ["track", id, ..] => (ServiceLinkKind::Track, *id),
            ["album", id, ..] => (ServiceLinkKind::Album, *id),
            ["artist", id, ..] => (ServiceLinkKind::Artist, *id),
            _ => return None,
        };
        if !id.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let seg = match kind {
            ServiceLinkKind::Track => "track",
            ServiceLinkKind::Album => "album",
            _ => "artist",
        };
        link(
            Service::Tidal,
            kind,
            Some(id),
            format!("https://tidal.com/{seg}/{id}"),
        )
    }

    fn qobuz(host: &str, segs: &[&str]) -> Option<ServiceLink> {
        if host == "open.qobuz.com" || host == "play.qobuz.com" {
            let (kind, id) = match segs {
                ["track", id, ..] => (ServiceLinkKind::Track, *id),
                ["album", id, ..] => (ServiceLinkKind::Album, *id),
                ["artist", id, ..] => (ServiceLinkKind::Artist, *id),
                _ => return None,
            };
            let seg = match kind {
                ServiceLinkKind::Track => "track",
                ServiceLinkKind::Album => "album",
                _ => "artist",
            };
            return link(
                Service::Qobuz,
                kind,
                Some(id),
                format!("https://open.qobuz.com/{seg}/{id}"),
            );
        }
        // www.qobuz.com/<locale>/album/<slug>/<id> or /interpreter/<slug>/<id>
        match segs {
            [_locale, "album", _slug, id, ..] => link(
                Service::Qobuz,
                ServiceLinkKind::Album,
                Some(id),
                format!("https://open.qobuz.com/album/{id}"),
            ),
            [_locale, "interpreter", _slug, id, ..] => link(
                Service::Qobuz,
                ServiceLinkKind::Artist,
                Some(id),
                format!("https://open.qobuz.com/artist/{id}"),
            ),
            _ => None,
        }
    }

    fn apple(url: &Url, segs: &[&str]) -> Option<ServiceLink> {
        // music.apple.com/<cc>/album/<slug>/<albumId>?i=<trackId>
        // music.apple.com/<cc>/artist/<slug>/<artistId>
        // music.apple.com/<cc>/song/<slug>/<trackId>
        let cc = segs.first()?;
        if cc.len() != 2 {
            return None;
        }
        let track_param = url
            .query_pairs()
            .find(|(k, _)| k == "i")
            .map(|(_, v)| v.to_string());
        match segs {
            [cc, "album", _slug, album_id, ..] => {
                let album_id = numeric(album_id)?;
                if let Some(tid) = track_param.as_deref().and_then(numeric) {
                    link(
                        Service::AppleMusic,
                        ServiceLinkKind::Track,
                        Some(tid),
                        format!("https://music.apple.com/{cc}/album/{album_id}?i={tid}"),
                    )
                } else {
                    link(
                        Service::AppleMusic,
                        ServiceLinkKind::Album,
                        Some(album_id),
                        format!("https://music.apple.com/{cc}/album/{album_id}"),
                    )
                }
            }
            [cc, "album", album_id] => {
                let album_id = numeric(album_id)?;
                link(
                    Service::AppleMusic,
                    ServiceLinkKind::Album,
                    Some(album_id),
                    format!("https://music.apple.com/{cc}/album/{album_id}"),
                )
            }
            [cc, "song", _, id, ..] | [cc, "song", id] => {
                let id = numeric(id)?;
                link(
                    Service::AppleMusic,
                    ServiceLinkKind::Track,
                    Some(id),
                    format!("https://music.apple.com/{cc}/song/{id}"),
                )
            }
            [cc, "artist", _, id, ..] | [cc, "artist", id] => {
                let id = numeric(id)?;
                link(
                    Service::AppleMusic,
                    ServiceLinkKind::Artist,
                    Some(id),
                    format!("https://music.apple.com/{cc}/artist/{id}"),
                )
            }
            _ => None,
        }
    }

    fn numeric(s: &str) -> Option<&str> {
        let s = s.strip_prefix("id").unwrap_or(s);
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            Some(s)
        } else {
            None
        }
    }

    fn deezer(segs: &[&str]) -> Option<ServiceLink> {
        // deezer.com/<lang>/track/<id> or deezer.com/track/<id>
        let segs: Vec<&str> = if segs.len() >= 3 && segs[0].len() == 2 {
            segs[1..].to_vec()
        } else {
            segs.to_vec()
        };
        let (kind, id) = match segs.as_slice() {
            ["track", id, ..] => (ServiceLinkKind::Track, *id),
            ["album", id, ..] => (ServiceLinkKind::Album, *id),
            ["artist", id, ..] => (ServiceLinkKind::Artist, *id),
            _ => return None,
        };
        let id = numeric(id)?;
        let seg = match kind {
            ServiceLinkKind::Track => "track",
            ServiceLinkKind::Album => "album",
            _ => "artist",
        };
        link(
            Service::Deezer,
            kind,
            Some(id),
            format!("https://www.deezer.com/{seg}/{id}"),
        )
    }

    fn youtube(url: &Url, segs: &[&str], service: Service) -> Option<ServiceLink> {
        let host = if service == Service::YouTubeMusic {
            "music.youtube.com"
        } else {
            "www.youtube.com"
        };
        match segs {
            ["watch"] => {
                let v = url
                    .query_pairs()
                    .find(|(k, _)| k == "v")
                    .map(|(_, v)| v.to_string())?;
                let kind = if service == Service::YouTubeMusic {
                    ServiceLinkKind::Track
                } else {
                    ServiceLinkKind::Unknown
                };
                link(service, kind, Some(&v), format!("https://{host}/watch?v={v}"))
            }
            ["playlist"] => {
                let list = url
                    .query_pairs()
                    .find(|(k, _)| k == "list")
                    .map(|(_, v)| v.to_string())?;
                let kind = if service == Service::YouTubeMusic && list.starts_with("OLAK5uy_") {
                    ServiceLinkKind::Album
                } else {
                    ServiceLinkKind::Unknown
                };
                link(
                    service,
                    kind,
                    Some(&list),
                    format!("https://{host}/playlist?list={list}"),
                )
            }
            ["channel", id, ..] => {
                let kind = if service == Service::YouTubeMusic {
                    ServiceLinkKind::Artist
                } else {
                    ServiceLinkKind::Unknown
                };
                link(service, kind, Some(id), format!("https://{host}/channel/{id}"))
            }
            ["browse", id, ..] if service == Service::YouTubeMusic => {
                let kind = if id.starts_with("MPREb_") {
                    ServiceLinkKind::Album
                } else if id.starts_with("UC") {
                    ServiceLinkKind::Artist
                } else {
                    ServiceLinkKind::Unknown
                };
                link(service, kind, Some(id), format!("https://{host}/browse/{id}"))
            }
            _ => None,
        }
    }

    fn youtu_be(segs: &[&str]) -> Option<ServiceLink> {
        let v = segs.first()?;
        link(
            Service::YouTube,
            ServiceLinkKind::Unknown,
            Some(v),
            format!("https://www.youtube.com/watch?v={v}"),
        )
    }

    fn amazon(url: &Url, host: &str, segs: &[&str]) -> Option<ServiceLink> {
        let track_asin = url
            .query_pairs()
            .find(|(k, _)| k == "trackAsin")
            .map(|(_, v)| v.to_string());
        match segs {
            ["albums", asin, ..] => {
                if let Some(t) = track_asin {
                    link(
                        Service::AmazonMusic,
                        ServiceLinkKind::Track,
                        Some(&t),
                        format!("https://{host}/albums/{asin}?trackAsin={t}"),
                    )
                } else {
                    link(
                        Service::AmazonMusic,
                        ServiceLinkKind::Album,
                        Some(asin),
                        format!("https://{host}/albums/{asin}"),
                    )
                }
            }
            ["tracks", asin, ..] => link(
                Service::AmazonMusic,
                ServiceLinkKind::Track,
                Some(asin),
                format!("https://{host}/tracks/{asin}"),
            ),
            ["artists", asin, ..] => link(
                Service::AmazonMusic,
                ServiceLinkKind::Artist,
                Some(asin),
                format!("https://{host}/artists/{asin}"),
            ),
            _ => None,
        }
    }

    fn beatport(segs: &[&str]) -> Option<ServiceLink> {
        let (kind, slug, id) = match segs {
            ["track", slug, id, ..] => (ServiceLinkKind::Track, slug, id),
            ["release", slug, id, ..] => (ServiceLinkKind::Album, slug, id),
            ["artist", slug, id, ..] => (ServiceLinkKind::Artist, slug, id),
            _ => return None,
        };
        let id = numeric(id)?;
        let seg = match kind {
            ServiceLinkKind::Track => "track",
            ServiceLinkKind::Album => "release",
            _ => "artist",
        };
        link(
            Service::Beatport,
            kind,
            Some(id),
            format!("https://www.beatport.com/{seg}/{slug}/{id}"),
        )
    }

    fn soundcloud(segs: &[&str]) -> Option<ServiceLink> {
        match segs {
            [user, "sets", slug, ..] => link(
                Service::SoundCloud,
                ServiceLinkKind::Album,
                None,
                format!("https://soundcloud.com/{user}/sets/{slug}"),
            ),
            [user, slug] => link(
                Service::SoundCloud,
                ServiceLinkKind::Track,
                None,
                format!("https://soundcloud.com/{user}/{slug}"),
            ),
            [user] => link(
                Service::SoundCloud,
                ServiceLinkKind::Artist,
                None,
                format!("https://soundcloud.com/{user}"),
            ),
            _ => None,
        }
    }

    fn bandcamp(host: &str, segs: &[&str]) -> Option<ServiceLink> {
        match segs {
            ["track", slug, ..] => link(
                Service::Bandcamp,
                ServiceLinkKind::Track,
                None,
                format!("https://{host}/track/{slug}"),
            ),
            ["album", slug, ..] => link(
                Service::Bandcamp,
                ServiceLinkKind::Album,
                None,
                format!("https://{host}/album/{slug}"),
            ),
            [] | ["music"] => link(
                Service::Bandcamp,
                ServiceLinkKind::Artist,
                None,
                format!("https://{host}/"),
            ),
            _ => None,
        }
    }

    /// `archive.org/details/<item>` is the item (an album-like thing);
    /// `archive.org/download/<item>/<file>` is one playable file (a track).
    fn archive(segs: &[&str]) -> Option<ServiceLink> {
        let ok_id = |id: &str| {
            !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        };
        match segs {
            ["details", id, ..] if ok_id(id) => link(
                Service::InternetArchive,
                ServiceLinkKind::Album,
                Some(id),
                format!("https://archive.org/details/{id}"),
            ),
            ["download", id, file] if ok_id(id) && !file.is_empty() => link(
                Service::InternetArchive,
                ServiceLinkKind::Track,
                Some(id),
                format!("https://archive.org/download/{id}/{file}"),
            ),
            _ => None,
        }
    }

    /// `audius.co/<handle>/<slug>` is a track; `audius.co/<handle>` an artist.
    fn audius(segs: &[&str]) -> Option<ServiceLink> {
        let ok = |s: &str| !s.is_empty() && !s.starts_with('?');
        match segs {
            [handle, slug] if ok(handle) && ok(slug) && *handle != "album" && *handle != "playlist" => link(
                Service::Audius,
                ServiceLinkKind::Track,
                None,
                format!("https://audius.co/{handle}/{slug}"),
            ),
            [handle, "album", slug] | [handle, "playlist", slug] if ok(handle) && ok(slug) => link(
                Service::Audius,
                ServiceLinkKind::Album,
                None,
                format!("https://audius.co/{handle}/{}/{slug}", segs[1]),
            ),
            [handle]
                if ok(handle)
                    && !matches!(*handle, "trending" | "explore" | "feed" | "signup" | "search") =>
            {
                link(
                    Service::Audius,
                    ServiceLinkKind::Artist,
                    None,
                    format!("https://audius.co/{handle}"),
                )
            }
            _ => None,
        }
    }

    fn audiomack(segs: &[&str]) -> Option<ServiceLink> {
        match segs {
            [artist, "song", slug, ..] => link(
                Service::Audiomack,
                ServiceLinkKind::Track,
                None,
                format!("https://audiomack.com/{artist}/song/{slug}"),
            ),
            [artist, "album", slug, ..] => link(
                Service::Audiomack,
                ServiceLinkKind::Album,
                None,
                format!("https://audiomack.com/{artist}/album/{slug}"),
            ),
            [artist] => link(
                Service::Audiomack,
                ServiceLinkKind::Artist,
                None,
                format!("https://audiomack.com/{artist}"),
            ),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: &str = "b1a9c0e9-d987-4042-ae91-78d6a3267d69";

    #[test]
    fn musicbrainz_roundtrip() {
        for (url, entity) in [
            (
                format!("https://musicbrainz.org/recording/{MB}"),
                MusicEntity::Recording,
            ),
            (
                format!("https://musicbrainz.org/release/{MB}"),
                MusicEntity::Release,
            ),
            (
                format!("https://musicbrainz.org/release-group/{MB}"),
                MusicEntity::ReleaseGroup,
            ),
            (
                format!("https://musicbrainz.org/artist/{MB}"),
                MusicEntity::Artist,
            ),
        ] {
            let u = MusicUri::parse(&url).unwrap();
            assert_eq!(u.entity(), Some(entity));
            assert!(u.is_canonical());
            assert_eq!(u.to_url(), url);
            assert_eq!(u.mbid(), Some(Mbid::parse(MB).unwrap()));
        }
        // Uppercase MBID and www. host normalize.
        let u = MusicUri::parse(&format!(
            "https://www.musicbrainz.org/recording/{}",
            MB.to_uppercase()
        ))
        .unwrap();
        assert_eq!(u.to_url(), format!("https://musicbrainz.org/recording/{MB}"));
    }

    #[test]
    fn isrc_fallback() {
        let u = MusicUri::parse("https://musicbrainz.org/isrc/gb-ayk-24-00123").unwrap();
        assert_eq!(u.to_url(), "https://musicbrainz.org/isrc/GBAYK2400123");
        assert!(!u.is_canonical());
        assert_eq!(u.entity(), Some(MusicEntity::Recording));
        assert_eq!(Isrc::parse("USRC17607839").unwrap().as_str(), "USRC17607839");
        assert!(Isrc::parse("USRC1760783").is_err());
        assert!(Isrc::parse("12RC17607839").is_err());
    }

    #[test]
    fn music_scheme_is_rejected() {
        assert_eq!(
            MusicUri::parse(&format!("music:recording:mbid:{MB}")),
            Err(IdError::MusicSchemeRetired)
        );
    }

    #[test]
    fn upc() {
        assert_eq!(Upc::parse("0 60254 71795 5").unwrap().as_str(), "060254717955");
        assert!(Upc::parse("abc").is_err());
    }

    #[test]
    fn audius_links() {
        let t = svc("https://audius.co/techwipe/ambient-piano-interlude-353852?utm=x");
        assert_eq!(t.service, Service::Audius);
        assert_eq!(t.kind, ServiceLinkKind::Track);
        assert_eq!(t.url, "https://audius.co/techwipe/ambient-piano-interlude-353852");
        assert_eq!(svc("https://audius.co/techwipe").kind, ServiceLinkKind::Artist);
        assert_eq!(
            svc("https://audius.co/techwipe/album/night-drive").kind,
            ServiceLinkKind::Album
        );
        assert!(MusicUri::parse("https://audius.co/trending").is_err());
    }

    #[test]
    fn internet_archive_links() {
        let item = svc("https://archive.org/details/gd1977-05-08.sbd.hicks.4982.sbeok.shnf?x=1");
        assert_eq!(item.service, Service::InternetArchive);
        assert_eq!(item.kind, ServiceLinkKind::Album);
        assert_eq!(
            item.url,
            "https://archive.org/details/gd1977-05-08.sbd.hicks.4982.sbeok.shnf"
        );
        let file = svc("https://archive.org/download/cornellgem/1.%20Minglewood%20Blues.mp3");
        assert_eq!(file.kind, ServiceLinkKind::Track);
        assert_eq!(file.id.as_deref(), Some("cornellgem"));
        assert_eq!(
            file.url,
            "https://archive.org/download/cornellgem/1.%20Minglewood%20Blues.mp3"
        );
        assert!(MusicUri::parse("https://archive.org/search?query=x").is_err());
        assert_eq!(Service::InternetArchive.label(), "internet-archive");
    }

    fn svc(url: &str) -> ServiceLink {
        match MusicUri::parse(url).unwrap() {
            MusicUri::Service(l) => l,
            other => panic!("expected service link, got {other:?}"),
        }
    }

    #[test]
    fn spotify() {
        let l = svc("https://open.spotify.com/intl-de/track/4cOdK2wGLETKBW3PvgPWqT?si=abc123");
        assert_eq!(l.service, Service::Spotify);
        assert_eq!(l.kind, ServiceLinkKind::Track);
        assert_eq!(l.url, "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT");
        assert_eq!(
            svc("https://open.spotify.com/album/1kCHru7uhxBUdzkm4gzRQc").kind,
            ServiceLinkKind::Album
        );
        assert_eq!(
            svc("https://open.spotify.com/artist/4tZwfgrHOc3mvqYlEYSvVi").kind,
            ServiceLinkKind::Artist
        );
    }

    #[test]
    fn tidal_qobuz_deezer() {
        assert_eq!(
            svc("https://tidal.com/browse/track/251380836?u").url,
            "https://tidal.com/track/251380836"
        );
        assert_eq!(
            svc("https://listen.tidal.com/album/251380835").kind,
            ServiceLinkKind::Album
        );
        assert_eq!(
            svc("https://tidal.com/artist/64518").kind,
            ServiceLinkKind::Artist
        );
        assert_eq!(
            svc("https://open.qobuz.com/track/60187542").url,
            "https://open.qobuz.com/track/60187542"
        );
        let q = svc("https://www.qobuz.com/gb-en/album/random-access-memories-daft-punk/0886443927087");
        assert_eq!(q.kind, ServiceLinkKind::Album);
        assert_eq!(q.url, "https://open.qobuz.com/album/0886443927087");
        let d = svc("https://www.deezer.com/en/track/67238735");
        assert_eq!(d.url, "https://www.deezer.com/track/67238735");
        assert_eq!(
            svc("https://deezer.com/album/6575789").kind,
            ServiceLinkKind::Album
        );
    }

    #[test]
    fn apple_music() {
        let t = svc("https://music.apple.com/us/album/get-lucky-feat-pharrell-williams-nile-rodgers/1440935467?i=1440935892");
        assert_eq!(t.kind, ServiceLinkKind::Track);
        assert_eq!(t.id.as_deref(), Some("1440935892"));
        assert_eq!(t.url, "https://music.apple.com/us/album/1440935467?i=1440935892");
        let a = svc("https://music.apple.com/us/album/random-access-memories/1440935467");
        assert_eq!(a.kind, ServiceLinkKind::Album);
        assert_eq!(a.url, "https://music.apple.com/us/album/1440935467");
        let ar = svc("https://music.apple.com/us/artist/daft-punk/5468295");
        assert_eq!(ar.kind, ServiceLinkKind::Artist);
    }

    #[test]
    fn youtube_variants() {
        let y = svc("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=42s");
        assert_eq!(y.service, Service::YouTube);
        assert_eq!(y.kind, ServiceLinkKind::Unknown);
        assert_eq!(y.url, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(
            svc("https://youtu.be/dQw4w9WgXcQ").url,
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        let m = svc("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RDAMVM");
        assert_eq!(m.service, Service::YouTubeMusic);
        assert_eq!(m.kind, ServiceLinkKind::Track);
        assert_eq!(
            svc("https://music.youtube.com/browse/MPREb_abc").kind,
            ServiceLinkKind::Album
        );
    }

    #[test]
    fn long_tail_services() {
        assert_eq!(
            svc("https://music.amazon.com/albums/B00CIQ0BG0?trackAsin=B00CIQ2JEW").kind,
            ServiceLinkKind::Track
        );
        assert_eq!(
            svc("https://music.amazon.de/albums/B00CIQ0BG0").kind,
            ServiceLinkKind::Album
        );
        assert_eq!(
            svc("https://www.beatport.com/track/get-lucky/4456163").url,
            "https://www.beatport.com/track/get-lucky/4456163"
        );
        assert_eq!(
            svc("https://soundcloud.com/daftpunkofficialmusic").kind,
            ServiceLinkKind::Artist
        );
        assert_eq!(
            svc("https://soundcloud.com/daftpunkofficialmusic/get-lucky").kind,
            ServiceLinkKind::Track
        );
        assert_eq!(
            svc("https://soundcloud.com/daftpunkofficialmusic/sets/ram").kind,
            ServiceLinkKind::Album
        );
        assert_eq!(
            svc("https://daftpunk.bandcamp.com/track/get-lucky").kind,
            ServiceLinkKind::Track
        );
        assert_eq!(
            svc("https://daftpunk.bandcamp.com/album/ram").kind,
            ServiceLinkKind::Album
        );
        assert_eq!(
            svc("https://audiomack.com/daft-punk/song/get-lucky").kind,
            ServiceLinkKind::Track
        );
    }

    #[test]
    fn unknown_host_and_scheme() {
        assert_eq!(
            MusicUri::parse("https://example.com/x"),
            Err(IdError::UnknownHost("example.com".into()))
        );
        assert_eq!(
            MusicUri::parse("ftp://open.spotify.com/track/x"),
            Err(IdError::UnsupportedScheme("ftp".into()))
        );
        assert!(matches!(MusicUri::parse("not a url"), Err(IdError::NotAUrl(_))));
    }

    #[test]
    fn serde_tagged() {
        let u = MusicUri::parse(&format!("https://musicbrainz.org/recording/{MB}")).unwrap();
        let json = serde_json::to_string(&u).unwrap();
        assert!(json.contains("\"kind\":\"recording\""));
        let back: MusicUri = serde_json::from_str(&json).unwrap();
        assert_eq!(u, back);
    }
}
