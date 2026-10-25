//! Sign-ins from other apps on this machine (STEWARD-CREDENTIALS-REUSE.md step 2).
//!
//! The node looks in the known places, read-only, says what it found (the app, the user, the
//! expiry; never a token) and uses one on a click. The same parsers take a pasted token file or a
//! bare token, for nodes that do not share a home directory with the other app (Docker, Umbrel).
//! What is imported is kept exactly like the node's own sign-in (the same setting, sealed in
//! backups, gone with Sign out) and renewed by the node where the issuing client is ours.
//!
//! Music Assistant's `settings.json` holds its sign-ins encrypted with MA's own key: it is on
//! the list of places so the person knows it was considered, and never read.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tidal_unofficial::Session;

/// A sign-in found in another app's files, as the chooser shows it. No token in here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Found {
    /// The app's key, also what `import {id}` takes: `tidal-dl-ng`, `beets`, `tidal-dl`,
    /// `streamrip`, or `pasted`.
    pub id: String,
    /// The app's name as people know it.
    pub app: String,
    /// Where it was found, with the home directory as `~`.
    pub place: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// The node can renew it: a refresh token issued by the client this build carries.
    pub renews: bool,
    pub usable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A found sign-in with what the import writes.
#[derive(Debug, Clone)]
pub(crate) struct Import {
    pub found: Found,
    pub session: Session,
}

/// Where the node looks for TIDAL sign-ins: `(id, app, path under the home directory)`.
/// tidal-dl-ng's config dir follows `XDG_CONFIG_HOME`; the rest are fixed.
const TIDAL_PLACES: &[(&str, &str, &str)] = &[
    ("tidal-dl-ng", "tidal-dl-ng", ".config/tidal_dl_ng/token.json"),
    ("beets", "beets (tidal plugin)", ".config/beets/tidal_token.json"),
    ("tidal-dl", "tidal-dl", ".tidal-dl.token.json"),
    ("streamrip", "streamrip", ".config/streamrip/config.toml"),
];

/// Apps whose files are listed but never read, with why.
pub const NOT_READ: &[(&str, &str)] = &[(
    "Music Assistant",
    "its settings.json keeps sign-ins encrypted with MA's own key",
)];

/// The app's name for a `Found::id` (`tidal-dl-ng` → "tidal-dl-ng", `pasted` → "Pasted token").
pub fn app_name(id: &str) -> &str {
    if id == "pasted" {
        return "Pasted token";
    }
    TIDAL_PLACES
        .iter()
        .find(|(i, _, _)| *i == id)
        .map(|(_, app, _)| *app)
        .unwrap_or(id)
}

/// The home directory the scan starts from; tests point it at a folder of their own.
pub static HOME_OVERRIDE: RwLock<Option<PathBuf>> = RwLock::new(None);

fn home() -> Option<PathBuf> {
    if let Some(h) = HOME_OVERRIDE.read().ok().and_then(|h| h.clone()) {
        return Some(h);
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The places looked at, for the chooser's note and the docs ("tidal-dl-ng: ~/.config/…").
pub fn tidal_places_text() -> Vec<String> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").is_some();
    TIDAL_PLACES
        .iter()
        .map(|(_, app, rel)| {
            let shown = if xdg && rel.starts_with(".config/") {
                format!("$XDG_CONFIG_HOME/{}", &rel[".config/".len()..])
            } else {
                format!("~/{rel}")
            };
            format!("{app}: {shown}")
        })
        .chain(
            NOT_READ
                .iter()
                .map(|(app, why)| format!("{app}: not read, {why}")),
        )
        .collect()
}

/// Where each app's file may be: under `XDG_CONFIG_HOME` when that is set, then the fixed path
/// under the home directory; the first that exists is read.
/// `(id, app, candidate paths with how each is shown)`.
type Place = (&'static str, &'static str, Vec<(PathBuf, String)>);

fn tidal_paths(home: &Path) -> Vec<Place> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    TIDAL_PLACES
        .iter()
        .map(|(id, app, rel)| {
            let mut candidates = Vec::new();
            if let (Some(x), Some(under)) = (&xdg, rel.strip_prefix(".config/")) {
                candidates.push((x.join(under), format!("$XDG_CONFIG_HOME/{under}")));
            }
            candidates.push((home.join(rel), format!("~/{rel}")));
            (*id, *app, candidates)
        })
        .collect()
}

/// Every TIDAL sign-in in the known places, usable or not (the chooser says why not).
pub(crate) fn scan_tidal() -> Vec<Import> {
    let Some(home) = home() else {
        return Vec::new();
    };
    tidal_paths(&home)
        .into_iter()
        .filter_map(|(id, app, candidates)| {
            let (text, shown) = candidates
                .into_iter()
                .find_map(|(path, shown)| Some((std::fs::read_to_string(&path).ok()?, shown)))?;
            Some(match parse_tidal(id, &text) {
                Ok(session) => import_of(id, app, &shown, session),
                Err(e) => Import {
                    found: Found {
                        id: id.into(),
                        app: app.into(),
                        place: shown,
                        user: None,
                        country: None,
                        expires_at: None,
                        renews: false,
                        usable: false,
                        reason: Some(e),
                    },
                    session: Session::default(),
                },
            })
        })
        .collect()
}

/// A pasted token file or bare token, as `import {text}` takes it.
pub(crate) fn pasted_tidal(text: &str) -> Result<Import, String> {
    let session = parse_tidal("pasted", text)?;
    let imp = import_of("pasted", "Pasted token", "pasted here", session);
    match &imp.found.reason {
        Some(why) => Err(why.clone()),
        None => Ok(imp),
    }
}

pub(crate) fn import_of(id: &str, app: &str, place: &str, session: Session) -> Import {
    let renews = session.refresh_token.as_deref().is_some_and(|r| !r.is_empty())
        && session.from.as_deref() != Some("tidal-dl")
        && session.from.as_deref() != Some("streamrip");
    let expired = session.expires_at > 0 && session.expires_at <= now();
    let reason = if session.access_token.trim().is_empty() {
        Some("no access token in it".to_string())
    } else if expired && !renews {
        Some(format!(
            "expired on {}{}",
            day(session.expires_at),
            if session.refresh_token.is_some() {
                ", and its refresh token belongs to that app's client, so this node cannot renew it"
            } else {
                ", with no refresh token"
            }
        ))
    } else {
        None
    };
    Import {
        found: Found {
            id: id.into(),
            app: app.into(),
            place: place.into(),
            user: session.user_id.clone(),
            country: (!session.country.is_empty()).then(|| session.country.clone()),
            expires_at: (session.expires_at > 0).then_some(session.expires_at),
            renews,
            usable: reason.is_none(),
            reason,
        },
        session,
    }
}

fn day(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .map(|t| format!("{:04}-{:02}-{:02}", t.year(), u8::from(t.month()), t.day()))
        .unwrap_or_else(|_| unix.to_string())
}

/// The claims of a JWT's payload, or null.
fn jwt_claims(token: &str) -> Value {
    use base64::Engine;
    token
        .split('.')
        .nth(1)
        .and_then(|p| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

/// Fills what the file left out from the token's own claims (`uid`, `cc`, `exp`).
fn with_claims(mut s: Session) -> Session {
    let c = jwt_claims(&s.access_token);
    if s.user_id.is_none() {
        s.user_id = c["uid"]
            .as_i64()
            .map(|u| u.to_string())
            .or_else(|| c["uid"].as_str().map(str::to_string));
    }
    if s.country.is_empty() {
        s.country = c["cc"].as_str().unwrap_or_default().to_string();
    }
    if s.expires_at == 0 {
        s.expires_at = c["exp"].as_i64().unwrap_or(0);
    }
    s
}

/// A TIDAL session from any of the known formats; `from` names the app for the status line.
/// Tried in turn: python-tidal's session file (tidal-dl-ng, beets), tidal-dl's token file
/// (base64 of JSON, or the JSON), streamrip's config (`[tidal]`), a bare access token with an
/// optional refresh token on the next line.
pub(crate) fn parse_tidal(from: &str, text: &str) -> Result<Session, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("nothing to read".into());
    }
    let tag = |mut s: Session| {
        s.from = Some(from.to_string());
        with_claims(s)
    };
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(s) = python_tidal(&v) {
            return Ok(tag(s));
        }
        if let Some(s) = tidal_dl(&v) {
            return Ok(tag(s));
        }
        return Err("a JSON file, but not a TIDAL token file this node knows".into());
    }
    {
        use base64::Engine;
        if let Some(v) = base64::engine::general_purpose::STANDARD
            .decode(text)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        {
            if let Some(s) = tidal_dl(&v).or_else(|| python_tidal(&v)) {
                return Ok(tag(s));
            }
        }
    }
    if let Some(s) = streamrip(text) {
        return Ok(tag(s));
    }
    if let Some(s) = bare_token(text) {
        return Ok(tag(s));
    }
    Err("not a TIDAL token file this node knows (tidal-dl-ng, beets, tidal-dl, streamrip) nor a bare access token".into())
}

/// python-tidal's `save_session_to_file`: `{"access_token": {"data": …}, …}`.
fn python_tidal(v: &Value) -> Option<Session> {
    let field = |k: &str| v.get(k).and_then(|x| x.get("data"));
    let access = field("access_token")?.as_str()?.to_string();
    let expires_at = field("expiry_time")
        .and_then(Value::as_str)
        .and_then(iso_to_unix)
        .unwrap_or(0);
    Some(Session {
        access_token: access,
        refresh_token: field("refresh_token").and_then(Value::as_str).map(str::to_string),
        expires_at,
        user_id: None,
        country: String::new(),
        session_id: field("session_id").and_then(Value::as_str).map(str::to_string),
        from: None,
    })
}

/// tidal-dl's token settings: `{userid, countryCode, accessToken, refreshToken, expiresAfter}`.
fn tidal_dl(v: &Value) -> Option<Session> {
    let access = v.get("accessToken")?.as_str()?.to_string();
    Some(Session {
        access_token: access,
        refresh_token: v["refreshToken"].as_str().map(str::to_string),
        expires_at: v["expiresAfter"]
            .as_i64()
            .or_else(|| v["expiresAfter"].as_f64().map(|f| f as i64))
            .unwrap_or(0),
        user_id: v["userid"]
            .as_i64()
            .map(|u| u.to_string())
            .or_else(|| v["userid"].as_str().map(str::to_string)),
        country: v["countryCode"].as_str().unwrap_or_default().to_string(),
        session_id: None,
        from: None,
    })
}

/// streamrip's `config.toml`, the `[tidal]` table: `user_id`, `country_code`, `access_token`,
/// `refresh_token`, `token_expiry` (unix seconds, as a string or a number). A flat reading of
/// `key = value` lines is enough; nothing else of the file is looked at.
fn streamrip(text: &str) -> Option<Session> {
    let mut in_tidal = false;
    let mut got = std::collections::BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_tidal = line == "[tidal]";
            continue;
        }
        if !in_tidal || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            let v = v.split(" #").next().unwrap_or(v).trim();
            got.insert(k.trim().to_string(), v.trim_matches(['"', '\'']).to_string());
        }
    }
    let access = got.get("access_token").filter(|a| !a.is_empty())?.clone();
    Some(Session {
        access_token: access,
        refresh_token: got.get("refresh_token").filter(|r| !r.is_empty()).cloned(),
        expires_at: got
            .get("token_expiry")
            .and_then(|e| e.parse::<f64>().ok())
            .map(|f| f as i64)
            .unwrap_or(0),
        user_id: got.get("user_id").filter(|u| !u.is_empty()).cloned(),
        country: got.get("country_code").cloned().unwrap_or_default(),
        session_id: None,
        from: None,
    })
}

/// A bare access token (a JWT), with an optional refresh token on the next line.
fn bare_token(text: &str) -> Option<Session> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let access = lines.next()?;
    if access.split('.').count() != 3 || access.contains(char::is_whitespace) {
        return None;
    }
    Some(Session {
        access_token: access.to_string(),
        refresh_token: lines.next().map(str::to_string),
        expires_at: 0,
        user_id: None,
        country: String::new(),
        session_id: None,
        from: None,
    })
}

/// `2026-10-25T12:34:56.123456` (naive, as Python's `isoformat()` writes it; read as UTC) or with
/// a `Z`/offset, to unix seconds.
fn iso_to_unix(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T').or_else(|| s.split_once(' '))?;
    let mut d = date.split('-');
    let y: i32 = d.next()?.parse().ok()?;
    let m: u8 = d.next()?.parse().ok()?;
    let day: u8 = d.next()?.parse().ok()?;
    let rest = rest.trim_end_matches('Z');
    let (clock, offset) = match rest.rfind(['+', '-']) {
        Some(i) if i >= 8 => (&rest[..i], Some(&rest[i..])),
        _ => (rest, None),
    };
    let mut t = clock.split('.').next()?.split(':');
    let h: u8 = t.next()?.parse().ok()?;
    let mi: u8 = t.next()?.parse().ok()?;
    let sec: u8 = t.next().unwrap_or("0").parse().ok()?;
    let date = time::Date::from_calendar_date(y, time::Month::try_from(m).ok()?, day).ok()?;
    let naive = date.with_hms(h, mi, sec).ok()?;
    let mut unix = naive.assume_utc().unix_timestamp();
    if let Some(o) = offset {
        let sign = if o.starts_with('-') { -1 } else { 1 };
        let (oh, om) = o[1..].split_once(':').unwrap_or((&o[1..], "0"));
        let secs: i64 = oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().unwrap_or(0) * 60;
        unix -= sign * secs;
    }
    Some(unix)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JWT with `uid`, `cc` and `exp` claims (the signature is not checked here).
    pub(crate) fn jwt(uid: i64, cc: &str, exp: i64) -> String {
        use base64::Engine;
        let b = |v: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.{}",
            b(r#"{"alg":"RS256","typ":"JWT"}"#),
            b(&format!(r#"{{"uid":{uid},"cc":"{cc}","exp":{exp}}}"#)),
            b("sig")
        )
    }

    #[test]
    fn python_tidal_session_files_are_read_with_the_user_from_the_token() {
        let text = format!(
            r#"{{"token_type": {{"data": "Bearer"}}, "session_id": {{"data": "s-1"}}, "access_token": {{"data": "{}"}}, "refresh_token": {{"data": "r-1"}}, "is_pkce": {{"data": false}}, "expiry_time": {{"data": "2096-10-25T12:34:56.123456"}}}}"#,
            jwt(4242, "DE", 4_000_000_000)
        );
        let s = parse_tidal("tidal-dl-ng", &text).unwrap();
        assert_eq!(s.user_id.as_deref(), Some("4242"));
        assert_eq!(s.country, "DE");
        assert_eq!(s.refresh_token.as_deref(), Some("r-1"));
        assert_eq!(s.session_id.as_deref(), Some("s-1"));
        assert_eq!(s.expires_at, iso_to_unix("2096-10-25T12:34:56").unwrap());
        assert_eq!(s.from.as_deref(), Some("tidal-dl-ng"));
        let f = import_of("tidal-dl-ng", "tidal-dl-ng", "~/x", s).found;
        assert!(f.usable && f.renews, "{f:?}");
    }

    #[test]
    fn tidal_dl_token_files_are_base64_json_and_cannot_be_renewed_here() {
        use base64::Engine;
        let json = format!(
            r#"{{"userid": 7, "countryCode": "US", "accessToken": "{}", "refreshToken": "r", "expiresAfter": 1000}}"#,
            jwt(7, "US", 1000)
        );
        let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        let s = parse_tidal("tidal-dl", &b64).unwrap();
        assert_eq!(s.user_id.as_deref(), Some("7"));
        assert_eq!(s.country, "US");
        let f = import_of("tidal-dl", "tidal-dl", "~/.tidal-dl.token.json", s).found;
        assert!(!f.usable && !f.renews, "{f:?}");
        assert!(
            f.reason.as_deref().unwrap().contains("expired on 1970-01-01")
                && f.reason.as_deref().unwrap().contains("that app's client"),
            "{f:?}"
        );
        // The plain JSON reads too
        assert!(parse_tidal("tidal-dl", &json).is_ok());
    }

    #[test]
    fn streamrip_config_and_bare_tokens_are_read() {
        let text = format!(
            "[qobuz]\nemail_or_userid = \"x\"\n[tidal]\nuser_id = \"99\"\ncountry_code = \"NL\" # set automatically\naccess_token = \"{}\"\nrefresh_token = \"rr\"\ntoken_expiry = \"4000000000.5\"\n[deezer]\narl = \"\"\n",
            jwt(99, "NL", 4_000_000_000)
        );
        let s = parse_tidal("streamrip", &text).unwrap();
        assert_eq!(s.user_id.as_deref(), Some("99"));
        assert_eq!(s.country, "NL");
        assert_eq!(s.expires_at, 4_000_000_000);
        let f = import_of("streamrip", "streamrip", "~/x", s).found;
        assert!(f.usable && !f.renews, "{f:?}");
        let s = parse_tidal("pasted", &format!("{}\n", jwt(5, "GB", 4_000_000_000))).unwrap();
        assert_eq!(s.user_id.as_deref(), Some("5"));
        assert_eq!(s.expires_at, 4_000_000_000);
        assert!(s.refresh_token.is_none());
        let s = parse_tidal("pasted", &format!("{}\nrefresh-1", jwt(5, "GB", 4_000_000_000))).unwrap();
        assert_eq!(s.refresh_token.as_deref(), Some("refresh-1"));
        for bad in ["", "hello world", "{\"a\": 1}", "[tidal]\nuser_id = \"1\"\n"] {
            assert!(parse_tidal("pasted", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn iso_times() {
        assert_eq!(iso_to_unix("1970-01-02T00:00:00"), Some(86400));
        assert_eq!(iso_to_unix("1970-01-02T00:00:00.5Z"), Some(86400));
        assert_eq!(iso_to_unix("1970-01-02T01:00:00+01:00"), Some(86400));
        assert_eq!(iso_to_unix("1970-01-01 23:00:00-01:00"), Some(86400));
        assert_eq!(iso_to_unix("garbage"), None);
    }
}
