//! Sign-ins from other apps on this machine (STEWARD-CREDENTIALS-REUSE.md step 2).
//!
//! The node looks in the known places, read-only, says what it found (the app, the user, the
//! expiry; never a token) and uses one on a click. The same parsers take a pasted token file or a
//! bare token, for nodes that do not share a home directory with the other app (Docker).
//! What is imported is kept exactly like the node's own sign-in (the same setting, sealed in
//! backups, gone with Sign out) and renewed by the node where the issuing client is ours.
//!
//! Music Assistant's `settings.json` holds its sign-ins encrypted with MA's own key: it is on
//! the list of places so the person knows it was considered, and never read.

use std::collections::BTreeMap;
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
/// Tests that set [`HOME_OVERRIDE`] hold this meanwhile, so two of them never race on it.
pub static HOME_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn home() -> Option<PathBuf> {
    if let Some(h) = HOME_OVERRIDE.read().ok().and_then(|h| h.clone()) {
        return Some(h);
    }
    // Tests must never inspect the developer's real sign-ins.
    #[cfg(test)]
    return None;
    #[cfg(not(test))]
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

// ---- shared: where to look, how to write what was picked ----

/// `(id, app, candidate paths with how each is shown)`.
type Candidates = (&'static str, &'static str, Vec<(PathBuf, String)>);

/// Each place's candidates under the home directory: `XDG_CONFIG_HOME` first for `.config/`
/// paths, then the fixed path; the first that exists is read.
fn candidates(places: &[(&'static str, &'static str, &'static str)], home: &Path) -> Vec<Candidates> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    places
        .iter()
        .map(|(id, app, rel)| {
            let mut c = Vec::new();
            if let (Some(x), Some(under)) = (&xdg, rel.strip_prefix(".config/")) {
                c.push((x.join(under), format!("$XDG_CONFIG_HOME/{under}")));
            }
            c.push((home.join(rel), format!("~/{rel}")));
            // The leaked strs live for the program: a handful of fixed place names
            (*id, *app, c)
        })
        .collect()
}

fn places_text(places: &[(&str, &str, &str)], not_read: &[(&str, &str)]) -> Vec<String> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").is_some();
    places
        .iter()
        .map(|(_, app, rel)| {
            let shown = match (xdg, rel.strip_prefix(".config/")) {
                (true, Some(under)) => format!("$XDG_CONFIG_HOME/{under}"),
                _ => format!("~/{rel}"),
            };
            format!("{app}: {shown}")
        })
        .chain(
            not_read
                .iter()
                .map(|(app, why)| format!("{app}: not read, {why}")),
        )
        .collect()
}

/// The first readable candidate's text and shown path.
fn first_readable(c: Vec<(PathBuf, String)>) -> Option<(String, String, PathBuf)> {
    c.into_iter()
        .find_map(|(path, shown)| Some((std::fs::read_to_string(&path).ok()?, shown, path)))
}

/// Write a secret (a sign-in, a session, a key), readable by the node's user only: created 0600
/// as a side file and renamed into place, so it is never readable by others, not even briefly,
/// and replaces a file there whatever that file's mode was.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    use std::io::Write;
    // Rename a completed private file so a failed write preserves the existing sign-in, and
    // an old symlink never makes us change the file that belongs to the other app.
    let temp = path.with_file_name(format!(".signin-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut f = o.open(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Forget a copied sign-in; a missing file is already signed out, other failures must be shown.
pub(crate) fn remove_private(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// `key = value` lines by section (`[name]`; lines before any section are in `""`), TOML and
/// INI alike: quotes stripped, `# …` comments dropped, arrays kept as their raw text.
fn sections(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut cur = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            cur = line.trim_matches(['[', ']']).trim().to_string();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            let v = if v.starts_with('[') {
                v.to_string()
            } else {
                v.split(" #")
                    .next()
                    .unwrap_or(v)
                    .trim()
                    .trim_matches(['"', '\''])
                    .to_string()
            };
            out.entry(cur.clone())
                .or_default()
                .insert(k.trim().to_string(), v);
        }
    }
    out
}

/// A TOML array `["a", "b"]` or an INI list `a,b` as its items.
fn list(v: Option<&String>) -> Vec<String> {
    v.map(|v| {
        v.trim_matches(['[', ']'])
            .split(',')
            .map(|x| x.trim().trim_matches(['"', '\'']).to_string())
            .filter(|x| !x.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

fn found(id: &str, app: &str, place: &str, user: Option<String>, reason: Option<String>) -> Found {
    Found {
        id: id.into(),
        app: app.into(),
        place: place.into(),
        user,
        country: None,
        expires_at: None,
        renews: false,
        usable: reason.is_none(),
        reason,
    }
}

// ---- Qobuz: streamrip's config.toml, qobuz-dl's config.ini ----

const QOBUZ_PLACES: &[(&str, &str, &str)] = &[
    ("streamrip", "streamrip", ".config/streamrip/config.toml"),
    ("qobuz-dl", "qobuz-dl", ".config/qobuz-dl/config.ini"),
];

/// A Qobuz sign-in from another app's config: the app values it uses, and a session token or
/// the email with the password's MD5 (what Qobuz's login takes; these files never hold the
/// password itself).
#[derive(Clone, Default)]
pub(crate) struct QobuzImport {
    pub app_id: String,
    /// What the app keeps; the first is used (Qobuz's web player publishes several).
    pub secrets: Vec<String>,
    pub email: Option<String>,
    pub password_md5: Option<String>,
    pub token: Option<String>,
    pub user_id: Option<String>,
}

impl std::fmt::Debug for QobuzImport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QobuzImport")
            .field("app_id", &self.app_id)
            .field("secrets", &self.secrets.len())
            .field("email", &self.email)
            .field("token", &self.token.as_ref().map(|_| "<set>"))
            .finish_non_exhaustive()
    }
}

pub(crate) fn parse_qobuz(text: &str) -> Result<QobuzImport, String> {
    let secs = sections(text);
    let (sec, streamrip) = if let Some(q) = secs.get("qobuz") {
        (q, true)
    } else if let Some(d) = secs.get("DEFAULT").filter(|d| d.contains_key("app_id")) {
        (d, false)
    } else {
        return Err(
            "not streamrip's config.toml ([qobuz]) nor qobuz-dl's config.ini ([DEFAULT] with app_id)".into(),
        );
    };
    let app_id = sec.get("app_id").cloned().unwrap_or_default();
    if app_id.is_empty() || app_id.len() > 20 || !app_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("no Qobuz app id in it".into());
    }
    let secrets: Vec<String> = list(sec.get("secrets"))
        .into_iter()
        .filter(|s| s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        .collect();
    if secrets.is_empty() {
        return Err("no Qobuz app secret in it".into());
    }
    let non_empty = |k: &str| sec.get(k).cloned().filter(|v| !v.is_empty());
    let mut imp = QobuzImport {
        app_id,
        secrets,
        ..Default::default()
    };
    if streamrip {
        let who = non_empty("email_or_userid");
        let what = non_empty("password_or_token");
        if sec.get("use_auth_token").is_some_and(|v| v == "true") {
            imp.token = what;
            imp.user_id = who;
        } else {
            imp.email = who;
            imp.password_md5 = what;
        }
    } else {
        imp.email = non_empty("email");
        imp.password_md5 = non_empty("password");
    }
    if imp.token.is_none() && (imp.email.is_none() || imp.password_md5.is_none()) {
        return Err("no sign-in in it: neither a token nor an email with the password's MD5".into());
    }
    if let Some(h) = &imp.password_md5 {
        if h.len() != 32 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(
                "the password field is not an MD5 (these apps keep the MD5, never the password)".into(),
            );
        }
    }
    Ok(imp)
}

pub(crate) fn qobuz_places_text() -> Vec<String> {
    places_text(QOBUZ_PLACES, &[])
}

/// Every Qobuz sign-in in the known places, usable or not.
pub(crate) fn scan_qobuz() -> Vec<(Found, Option<QobuzImport>)> {
    let Some(home) = home() else {
        return Vec::new();
    };
    candidates(QOBUZ_PLACES, &home)
        .into_iter()
        .filter_map(|(id, app, c)| {
            let (text, shown, _) = first_readable(c)?;
            Some(match parse_qobuz(&text) {
                Ok(imp) => (
                    found(
                        id,
                        app,
                        &shown,
                        imp.email.clone().or_else(|| imp.user_id.clone()),
                        None,
                    ),
                    Some(imp),
                ),
                Err(e) => (found(id, app, &shown, None, Some(e)), None),
            })
        })
        .collect()
}

// ---- Last.fm: rescrobbled ----

const LASTFM_PLACES: &[(&str, &str, &str)] =
    &[("rescrobbled", "rescrobbled", ".config/rescrobbled/config.toml")];

/// A Last.fm sign-in from another app: its API key and shared secret (a session key only works
/// with the key that made it) and the session key.
#[derive(Clone)]
pub(crate) struct LastfmImport {
    pub api_key: String,
    pub api_secret: String,
    pub session_key: String,
}

impl std::fmt::Debug for LastfmImport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LastfmImport")
            .field("api_key", &self.api_key)
            .finish_non_exhaustive()
    }
}

fn hex32(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Three lines: API key, shared secret, session key; or rescrobbled's `lastfm-key = …` /
/// `lastfm-secret = …` lines with the session key on a line of its own.
pub(crate) fn parse_lastfm(text: &str) -> Result<LastfmImport, String> {
    let secs = sections(text);
    let top = secs.get("").cloned().unwrap_or_default();
    let bare: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.contains('='))
        .collect();
    let (key, secret, session) = match (top.get("lastfm-key"), top.get("lastfm-secret")) {
        (Some(k), Some(s)) => (k.clone(), s.clone(), bare.first().map(|x| x.to_string())),
        _ => match bare.as_slice() {
            [k, s, sk, ..] => (k.to_string(), s.to_string(), Some(sk.to_string())),
            _ => {
                return Err(
                    "three lines are needed: the API key, the shared secret and the session key".into(),
                )
            }
        },
    };
    if !hex32(&key) || !hex32(&secret) {
        return Err("the API key and shared secret are 32 hexadecimal characters each".into());
    }
    let session = session.filter(|s| {
        !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    });
    let Some(session_key) = session else {
        return Err("no session key in it (rescrobbled keeps it in ~/.config/rescrobbled/session)".into());
    };
    Ok(LastfmImport {
        api_key: key,
        api_secret: secret,
        session_key,
    })
}

pub(crate) fn lastfm_places_text() -> Vec<String> {
    let mut v = places_text(LASTFM_PLACES, &[]);
    v.push("rescrobbled: the session key in ~/.config/rescrobbled/session beside it".into());
    v.push("mpdscribble: not read, it keeps a password, not a session".into());
    v
}

/// rescrobbled's sign-in: key and secret from config.toml (or the files it names), the session
/// key from `session` beside it.
pub(crate) fn scan_lastfm() -> Vec<(Found, Option<LastfmImport>)> {
    let Some(home) = home() else {
        return Vec::new();
    };
    candidates(LASTFM_PLACES, &home)
        .into_iter()
        .filter_map(|(id, app, c)| {
            let (text, shown, path) = first_readable(c)?;
            let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
            let top = sections(&text).get("").cloned().unwrap_or_default();
            let read_named = |k: &str| -> Option<String> {
                let v = top.get(k)?;
                let p = if let Some(rest) = v.strip_prefix("~/") {
                    home.join(rest)
                } else {
                    dir.join(v)
                };
                std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
            };
            let key = top
                .get("lastfm-key")
                .cloned()
                .or_else(|| read_named("lastfm-key-file"));
            let secret = top
                .get("lastfm-secret")
                .cloned()
                .or_else(|| read_named("lastfm-secret-file"));
            let session = std::fs::read_to_string(dir.join("session"))
                .ok()
                .map(|s| s.trim().to_string());
            let joined = format!(
                "{}\n{}\n{}\n",
                key.unwrap_or_default(),
                secret.unwrap_or_default(),
                session.unwrap_or_default()
            );
            Some(match parse_lastfm(&joined) {
                Ok(imp) => (found(id, app, &shown, None, None), Some(imp)),
                Err(e) => (found(id, app, &shown, None, Some(e)), None),
            })
        })
        .collect()
}

// ---- librespot's credentials.json, yt-dlp's cookies: picked, never looked for ----

/// Where these would be, said in the chooser; the node does not look there.
pub(crate) fn librespot_places_text() -> Vec<String> {
    vec![
        "librespot credentials.json: not looked for; pick the file yourself (spotifyd: ~/.cache/spotifyd/credentials.json, raspotify: /var/cache/raspotify/credentials.json, go-librespot: its config folder)".into(),
    ]
}

pub(crate) fn cookies_places_text() -> Vec<String> {
    vec![
        "yt-dlp cookies: not looked for; pick the Netscape cookies file you export for yt-dlp (--cookies)"
            .into(),
    ]
}

/// A librespot `credentials.json`: `{username?, auth_type: <int>, auth_data: <base64>}`.
pub(crate) struct LibrespotCredentials {
    /// The file as given, trimmed.
    pub json: String,
}

pub(crate) fn parse_librespot_credentials(text: &str) -> Result<LibrespotCredentials, String> {
    use base64::Engine;
    if text.len() > 16 * 1024 {
        return Err("too large for a credentials.json".into());
    }
    let v: Value = serde_json::from_str(text.trim()).map_err(|_| "not a JSON file".to_string())?;
    let o = v.as_object().ok_or("not a JSON object")?;
    if !matches!(o.get("auth_type").and_then(Value::as_i64), Some(0..=4)) {
        return Err("unknown auth_type: not librespot's credentials.json".into());
    }
    let data = o
        .get("auth_data")
        .and_then(Value::as_str)
        .ok_or("no auth_data in it: not librespot's credentials.json")?;
    if !base64::engine::general_purpose::STANDARD
        .decode(data)
        .is_ok_and(|d| !d.is_empty())
    {
        return Err("auth_data must be non-empty base64".into());
    }
    if o.get("username").is_some_and(|u| !u.is_null() && !u.is_string()) {
        return Err("username is not a string".into());
    }
    Ok(LibrespotCredentials {
        json: text.trim().to_string(),
    })
}

/// A Netscape cookies file with YouTube cookies in it: how many.
pub(crate) fn check_cookies(text: &str) -> Result<usize, String> {
    if text.len() > 1024 * 1024 {
        return Err("too large for a cookies file".into());
    }
    if !text
        .lines()
        .next()
        .is_some_and(|l| l.starts_with("# Netscape HTTP Cookie File") || l.starts_with("# HTTP Cookie File"))
    {
        return Err("not a Netscape cookies file (missing the HTTP Cookie File header)".into());
    }
    let mut rows = 0;
    let mut youtube = 0;
    let mut live = 0;
    for line in text.lines() {
        // HttpOnly is part of a cookie row, not a comment. Preserve empty final values.
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 7 {
            return Err("not a Netscape cookies file (seven tab-separated columns per cookie)".into());
        }
        if !matches!(fields[1], "TRUE" | "FALSE")
            || !matches!(fields[3], "TRUE" | "FALSE")
            || !fields[2].starts_with('/')
        {
            return Err("invalid Netscape cookie flags or path".into());
        }
        let expiry = if fields[4].is_empty() {
            0
        } else {
            fields[4].parse::<i64>().map_err(|_| "invalid cookie expiry")?
        };
        rows += 1;
        let domain = fields[0].trim_start_matches('.').to_ascii_lowercase();
        if ["youtube.com", "google.com"]
            .iter()
            .any(|d| domain == *d || domain.ends_with(&format!(".{d}")))
        {
            youtube += 1;
            if expiry == 0 || expiry > now() {
                live += 1;
            }
        }
    }
    if rows == 0 {
        return Err("no cookies in it".into());
    }
    if youtube == 0 {
        return Err("no youtube.com or google.com cookies in it".into());
    }
    if live == 0 {
        return Err("the YouTube/Google cookies have expired; export a new file or try another way".into());
    }
    Ok(youtube)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn replacing_a_private_copy_preserves_an_original_behind_a_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("other-app.json");
        let copy = dir.path().join("credentials.json");
        std::fs::write(&original, b"original fixture").unwrap();
        symlink(&original, &copy).unwrap();
        write_private(&copy, b"replacement fixture").unwrap();
        assert_eq!(std::fs::read(&original).unwrap(), b"original fixture");
        assert_eq!(std::fs::read(&copy).unwrap(), b"replacement fixture");
        assert_eq!(
            std::fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o600
        );
        remove_private(&copy).unwrap();
        assert!(original.exists());
    }

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

    #[test]
    fn qobuz_configs_from_streamrip_and_qobuz_dl_are_read() {
        let sr = "[qobuz]\nuse_auth_token = false\nemail_or_userid = \"me@example.org\"\npassword_or_token = \"00000000000000000000000000000000\"\napp_id = \"100000000\"\nsecrets = [\"s3cret\", \"other1\"]\n[tidal]\nuser_id = \"1\"\n";
        let q = parse_qobuz(sr).unwrap();
        assert_eq!(
            (
                q.app_id.as_str(),
                q.secrets.len(),
                q.email.as_deref(),
                q.token.is_none()
            ),
            ("100000000", 2, Some("me@example.org"), true)
        );
        let srt = sr
            .replace("use_auth_token = false", "use_auth_token = true")
            .replace("me@example.org", "42")
            .replace("00000000000000000000000000000000", "tok-abc");
        let q = parse_qobuz(&srt).unwrap();
        assert_eq!(
            (q.token.as_deref(), q.user_id.as_deref(), q.email.is_none()),
            (Some("tok-abc"), Some("42"), true)
        );
        let ini = "[DEFAULT]\nemail = me@example.org\npassword = 00000000000000000000000000000000\napp_id = 100000000\nsecrets = s3cret,other1\ndefault_folder = Qobuz Downloads\n";
        let q = parse_qobuz(ini).unwrap();
        assert_eq!(
            (q.secrets[0].as_str(), q.password_md5.is_some()),
            ("s3cret", true)
        );
        for bad in [
            "",
            "[qobuz]\napp_id = \"1\"\n",
            "[DEFAULT]\napp_id = 1\nsecrets = a\nemail = x\npassword = plain\n",
        ] {
            assert!(parse_qobuz(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn lastfm_sign_ins_and_the_picked_files_are_checked() {
        let three = "0123456789abcdef0123456789abcdef\nfedcba9876543210fedcba9876543210\nsessKey12345\n";
        let l = parse_lastfm(three).unwrap();
        assert_eq!(l.session_key, "sessKey12345");
        let cfg = "lastfm-key = \"0123456789abcdef0123456789abcdef\"\nlastfm-secret = \"fedcba9876543210fedcba9876543210\"\nmin-play-time = 0\nsessKey12345\n";
        assert_eq!(
            parse_lastfm(cfg).unwrap().api_key,
            "0123456789abcdef0123456789abcdef"
        );
        assert!(parse_lastfm("a\nb\nc\n").is_err());
        assert!(
            parse_lastfm("0123456789abcdef0123456789abcdef\nfedcba9876543210fedcba9876543210\n").is_err()
        );
        let creds = r#"{"username": "alice", "auth_type": 1, "auth_data": "QUJD"}"#;
        assert!(parse_librespot_credentials(creds).unwrap().json.contains("alice"));
        assert!(parse_librespot_credentials(r#"{"username": "alice"}"#).is_err());
        assert!(parse_librespot_credentials("nope").is_err());
        assert!(parse_librespot_credentials(&creds.replace("\"QUJD\"", "\"\"")).is_err());
        assert!(
            parse_librespot_credentials(&creds.replace("\"auth_type\": 1", "\"auth_type\": 99")).is_err()
        );
        assert!(parse_librespot_credentials(&creds.replace("\"alice\"", "null")).is_ok());
        let cookies = "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t2000000000\tSID\tabc\n.example.com\tTRUE\t/\tFALSE\t0\tx\ty\n";
        assert_eq!(check_cookies(cookies).unwrap(), 1);
        assert_eq!(
            check_cookies(&cookies.replace(".youtube.com", "#HttpOnly_.youtube.com")).unwrap(),
            1
        );
        assert!(check_cookies(&cookies.replace(".youtube.com", ".notyoutube.com")).is_err());
        assert!(check_cookies(&cookies.replace("2000000000", "1"))
            .unwrap_err()
            .contains("expired"));
        assert!(check_cookies(&cookies.replace("2000000000", "soon")).is_err());
        assert!(check_cookies(&cookies.replace("2000000000", "")).is_ok());
        assert!(check_cookies(&cookies.replace("TRUE", "yes")).is_err());
        assert!(check_cookies(".example.com\tTRUE\t/\tFALSE\t0\tx\ty\n").is_err());
        assert!(check_cookies("not\tenough\tcolumns\n").is_err());
        assert!(check_cookies("").is_err());
    }
}
