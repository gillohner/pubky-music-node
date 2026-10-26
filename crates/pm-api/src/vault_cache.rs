//! The node's sign-ins as a cache of the vault (STEWARD-VAULT.md item 6).
//!
//! Each source keeps its sign-in in the node's settings as it always has, and reads it there
//! unchanged. This module keeps those settings and the person's vault in step, one service at a
//! time ([`decide`]):
//! - a sign-in only on the node moves into the vault once (the migration); one only in the vault
//!   (made on another device or node) fills the node's settings;
//! - after that, whichever side changed since the last sync wins, by content, not by revision (a
//!   sign-out rotation bumps the revision without changing anything, and a blob made again after
//!   "Sign out everywhere" starts at revision 1);
//! - signing out of a source on the node removes it from the vault; a sign-in removed from the
//!   vault elsewhere is moved aside on the node (`vault.signed_out.<key>`, never in a backup);
//! - two different copies meeting for the first time are a conflict when they are for different
//!   accounts: both stay as they are, and the report names it (`POST /v1/vault/sync {service,
//!   take}` settles it); copies of one account are one sign-in refreshed on two sides.
//!
//! A refresh and a new sign-in are told apart by an id the sources stamp at every interactive
//! sign-in ([`new_sign_in`]), carried in the blob (`node.lineage`). Each write or removal is
//! checked again under the blob's lock against the copy it was decided on, so a pass on another
//! node meanwhile isn't undone. The sync records belong to the machine (they name its vault key)
//! and never go into a backup.
//!
//! "Sign out everywhere" is never undone: the pass right after it moves every copy on that node
//! aside, and other nodes move aside any copy made before it (the sign-in ids carry their time;
//! the device list carries `signed_out_everywhere_at`), conflicts included.
//!
//! Known limits: a sign-in lost to a change of the node's app is forgotten, not signed out, even
//! when the person signed out by hand after the app changed. Two nodes that each had the same
//! account end up on one refresh chain; with a service that rotates refresh tokens, the node that
//! didn't refresh last fails until its next pass takes the new tokens.
//!
//! Only the sign-in travels: the members of each setting that hold the credential ([`Carry`]).
//! What belongs to the machine stays: whether a mode is switched on, its health, a pending
//! device login, the download format. A sign-in made with another app than the node's (Spotify,
//! TIDAL, Qobuz, Last.fm and Jamendo tokens belong to the app that made them) isn't pulled.
//!
//! Not bridged yet, and why: media servers (Plex, Jellyfin, OpenSubsonic, Audiobookshelf: usually a
//! server in that house, and Jellyfin's device id is per node), drives (their accounts live in
//! memory), gPodder (its sync cursors are per node), SoundCloud (the person's token sits in the
//! admin's client setting). Librespot's credentials file and yt-dlp's state stay on the machine.

use crate::state::NodeState;
use pm_library::Library;
use pm_pubky::vault::{Opened, Vault, VaultStoreError};
use pubky_music_specs::vault::{ServiceUser, SignIn, VaultError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// What of a setting travels with the sign-in.
pub enum Part {
    /// The whole value (a session object, a cookie).
    Whole,
    /// These members of a JSON object; the rest stays on the node.
    Members(&'static [&'static str]),
}

/// One setting a sign-in lives in.
pub struct Carry {
    pub key: &'static str,
    pub part: Part,
}

/// One sign-in the vault carries: its id there, the settings it lives in (the first is the
/// sign-in itself: no sign-in without it), and the app its tokens belong to.
pub struct Bridge {
    pub service: &'static str,
    pub carry: &'static [Carry],
    pub app: Option<fn(&NodeState) -> Option<String>>,
    /// Members of the node's state about the sign-in it had (`setting`, `member`), dropped when
    /// it takes another one from the vault: Last.fm's "this key was refused".
    pub reset: &'static [(&'static str, &'static str)],
}

const fn whole(key: &'static str) -> Carry {
    Carry {
        key,
        part: Part::Whole,
    }
}

const fn members(key: &'static str, m: &'static [&'static str]) -> Carry {
    Carry {
        key,
        part: Part::Members(m),
    }
}

/// Every sign-in the node keeps for the person, by source. Only the person's own sign-ins: the
/// node's apps (Spotify client id, Qobuz app, SoundCloud client, Last.fm and YouTube keys, a TIDAL
/// client of the admin's) are set once by the admin and never enter a vault (ADR 0016).
pub const BRIDGES: &[Bridge] = &[
    Bridge {
        service: "tidal",
        carry: &[whole("tidal.user")],
        app: Some(crate::tidal::app_id),
        reset: &[],
    },
    Bridge {
        service: "tidal-full",
        carry: &[members("tidal.unofficial", &["session"])],
        app: Some(crate::tidal_unofficial::app_id),
        reset: &[],
    },
    Bridge {
        service: "spotify",
        carry: &[whole("spotify.user")],
        app: Some(crate::spotify::app_id),
        reset: &[],
    },
    Bridge {
        service: "qobuz",
        carry: &[members("qobuz.unofficial", &["session"])],
        app: Some(crate::qobuz::app_id),
        reset: &[],
    },
    Bridge {
        service: "bandcamp",
        carry: &[
            whole("bandcamp.identity"),
            members("bandcamp.unofficial", &["fan"]),
        ],
        app: None,
        reset: &[],
    },
    Bridge {
        service: "lastfm",
        // The key and who it is; whether to send what plays (`submit`) stays per node
        carry: &[members("lastfm.session", &["key", "user"])],
        app: Some(crate::lastfm::app_id),
        reset: &[("lastfm.session", "problem")],
    },
    Bridge {
        service: "listenbrainz",
        carry: &[members("listenbrainz.account", &["token", "user"])],
        app: None,
        reset: &[],
    },
    Bridge {
        service: "jamendo",
        carry: &[whole("jamendo.user")],
        app: Some(crate::jamendo::app_id),
        reset: &[],
    },
];

/// Where a moved-aside copy goes (a backup leaves these out, like the sign-ins themselves).
pub const SIGNED_OUT_PREFIX: &str = "vault.signed_out.";

/// What the node last synced for a service.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Synced {
    /// The vault revision then (shown, not compared).
    #[serde(default)]
    pub revision: u64,
    /// The hash of what travels, the same on both sides then.
    pub hash: String,
    /// The account it was for, when the sign-in names one.
    #[serde(default)]
    pub account: Option<String>,
    /// The Pubky account whose vault it was.
    #[serde(default)]
    pub owner: String,
    /// The two sides differed (a conflict): `hash` is the node's copy then, and nothing is taken
    /// until they agree; a sign-out everywhere still moves the node's copy aside.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conflict: bool,
    /// Which sign-in it was: a refresh keeps this, a new sign-in changes it ([`new_sign_in`]).
    #[serde(default)]
    pub lineage: Option<String>,
    /// This node's vault key then: a record from another machine (a restored backup) counts
    /// for nothing.
    #[serde(default)]
    pub client: String,
    /// The node's app for the service then: a sign-in lost to a change of app isn't a sign-out.
    #[serde(default)]
    pub app: Option<String>,
    /// When it was recorded (Unix seconds).
    #[serde(default)]
    pub at: u64,
    /// The device list's `signed_out_everywhere_at` then: a different one now means the person
    /// signed out everywhere since (compared as a value, not against this machine's clock).
    #[serde(default)]
    pub marker: Option<u64>,
}

fn synced_key(service: &str) -> String {
    format!("vault.synced.{service}")
}

fn lineage_key(service: &str) -> String {
    format!("vault.lineage.{service}")
}

/// A source signed in interactively (not a refresh): its sign-in gets a new id, so the vault can
/// tell it from a refresh of the old one.
pub fn new_sign_in(lib: &Library, service: &str) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let id = blake3::hash(format!("{t}.{n}.{}.{service}", std::process::id()).as_bytes()).to_hex();
    let at = pm_library::db::now_unix().max(0);
    let _ = lib.set_setting(&lineage_key(service), &format!("{at}-{}", &id.as_str()[..16]));
}

/// When a sign-in id was made (Unix seconds; 0 for an id made for a sign-in that was already
/// there, or one from before ids had a time).
pub fn lineage_at(lineage: &str) -> u64 {
    lineage
        .split_once('-')
        .and_then(|(t, _)| t.parse().ok())
        .unwrap_or(0)
}

/// [`new_sign_in`] for a source that doesn't hold the library.
pub fn new_sign_in_of(s: &NodeState, service: &str) {
    new_sign_in(&s.library.lock().expect("library poisoned"), service);
}

/// The id of the node's sign-in for a service, made the first time it is asked for (time 0: the
/// sign-in was already there, nobody knows since when).
fn local_lineage(lib: &Library, service: &str) -> Result<String, String> {
    if let Some(l) = lib
        .get_setting(&lineage_key(service))
        .map_err(|e| e.to_string())?
    {
        return Ok(l);
    }
    new_sign_in(lib, service);
    let made = lib
        .get_setting(&lineage_key(service))
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let id = made
        .split_once('-')
        .map_or(made.as_str(), |(_, id)| id)
        .to_string();
    let l = format!("0-{id}");
    lib.set_setting(&lineage_key(service), &l)
        .map_err(|e| e.to_string())?;
    Ok(l)
}

/// What one sync did, per service.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct CacheReport {
    /// Written to the vault for the first time.
    pub migrated: Vec<String>,
    /// The node's copy changed: written to the vault.
    pub pushed: Vec<String>,
    /// The vault's copy changed (or was new): the node's settings now hold it.
    pub pulled: Vec<String>,
    /// Removed from the vault elsewhere: moved aside on the node.
    pub signed_out: Vec<String>,
    /// Signed out on this node: removed from the vault.
    pub removed: Vec<String>,
    /// Two different copies, neither known to be the newer: both left as they are.
    pub conflicts: Vec<String>,
    /// In the vault, but not usable here: made with another app, or written by a client that
    /// isn't a node.
    pub not_usable: Vec<String>,
    /// In the vault, but this node can't read it (another device must share it).
    pub not_readable: Vec<String>,
    /// Couldn't be synced this time (the homeserver, or the vault changed meanwhile); the next
    /// pass tries again.
    pub failed: Vec<String>,
    /// Lost on this node without a sign-out (its app for the service changed): left in the vault.
    pub forgotten: Vec<String>,
}

// ---- what travels ----

/// A setting's value as a JSON object, when it is one.
fn object_of(v: &Value) -> Option<Map<String, Value>> {
    match v {
        Value::Object(o) => Some(o.clone()),
        Value::String(raw) => match serde_json::from_str::<Value>(raw).ok()? {
            Value::Object(o) => Some(o),
            _ => None,
        },
        _ => None,
    }
}

/// What of the node's settings travels for a bridge (`None`: not signed in here). Whole values are
/// kept as the stored string, members as an object.
/// A setting that can't be read is an error, never "not signed in".
pub fn carried(lib: &Library, b: &Bridge) -> Result<Option<Map<String, Value>>, String> {
    let mut m = Map::new();
    for (i, c) in b.carry.iter().enumerate() {
        let raw = lib
            .get_setting(c.key)
            .map_err(|e| format!("{}: {e}", c.key))?
            .filter(|v| !v.trim().is_empty());
        let part = match (&c.part, raw) {
            (Part::Whole, Some(raw)) => Some(Value::String(raw)),
            (Part::Members(ms), Some(raw)) => {
                let obj = object_of(&Value::String(raw)).unwrap_or_default();
                let sub: Map<String, Value> = ms
                    .iter()
                    .filter_map(|k| {
                        obj.get(*k)
                            .filter(|v| !v.is_null())
                            .map(|v| ((*k).to_string(), v.clone()))
                    })
                    .collect();
                (!sub.is_empty()).then_some(Value::Object(sub))
            }
            (_, None) => None,
        };
        match part {
            Some(v) => {
                m.insert(c.key.to_string(), v);
            }
            None if i == 0 => return Ok(None),
            None => {}
        }
    }
    Ok(Some(m))
}

/// Put what travels into the node's settings: whole values replace the setting, members replace
/// those members and leave the rest of the object. What the copy doesn't have is removed.
pub fn apply(lib: &Library, b: &Bridge, travels: &Map<String, Value>) {
    for c in b.carry {
        match &c.part {
            Part::Whole => match travels.get(c.key) {
                Some(Value::String(v)) => {
                    let _ = lib.set_setting(c.key, v);
                }
                Some(other) => {
                    let _ = lib.set_setting(c.key, &other.to_string());
                }
                None => {
                    let _ = lib.delete_setting(c.key);
                }
            },
            Part::Members(ms) => {
                let raw = lib.get_setting(c.key).ok().flatten();
                let mut obj = raw
                    .as_ref()
                    .and_then(|r| object_of(&Value::String(r.clone())))
                    .unwrap_or_default();
                let from = travels.get(c.key).and_then(object_of).unwrap_or_default();
                for k in *ms {
                    match from.get(*k) {
                        Some(v) => {
                            obj.insert((*k).to_string(), v.clone());
                        }
                        None => {
                            obj.remove(*k);
                        }
                    }
                }
                if obj.is_empty() {
                    let _ = lib.delete_setting(c.key);
                } else {
                    let _ = lib.set_setting(c.key, &Value::Object(obj).to_string());
                }
            }
        }
    }
}

/// Signed out elsewhere: what travels moves to `vault.signed_out.<key>` (kept, not used) and
/// goes from the setting.
pub fn move_aside(lib: &Library, b: &Bridge) {
    let Ok(Some(travels)) = carried(lib, b) else {
        return;
    };
    let _ = lib.delete_setting(&lineage_key(b.service));
    for (key, v) in &travels {
        let aside = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let _ = lib.set_setting(&format!("{SIGNED_OUT_PREFIX}{key}"), &aside);
    }
    apply(lib, b, &Map::new());
}

/// What the node noted about the sign-in it had ([`Bridge::reset`]) doesn't apply to one it
/// just took from the vault.
pub fn reset_after_pull(lib: &Library, b: &Bridge) {
    for (key, member) in b.reset {
        if let Ok(Some(raw)) = lib.get_setting(key) {
            if let Some(mut obj) = object_of(&Value::String(raw)) {
                if obj.remove(*member).is_some() {
                    let _ = lib.set_setting(key, &Value::Object(obj).to_string());
                }
            }
        }
    }
}

pub fn hash(m: &Map<String, Value>) -> String {
    blake3::hash(serde_json::to_string(m).unwrap_or_default().as_bytes())
        .to_hex()
        .to_string()
}

/// The account a copy is for, from the usual members of what travels (`user_id`, `id`,
/// `username`, `user`, Jamendo's `name`, Bandcamp's `fan.fan_id`), looking into a `session` too.
pub fn account(travels: &Map<String, Value>) -> Option<String> {
    fn find(o: &Map<String, Value>) -> Option<String> {
        for k in ["user_id", "id", "username", "user", "name"] {
            match o.get(k) {
                Some(Value::String(v)) if !v.is_empty() => return Some(v.clone()),
                Some(Value::Number(n)) => return Some(n.to_string()),
                _ => {}
            }
        }
        for k in ["session", "fan"] {
            if let Some(Value::Object(inner)) = o.get(k) {
                if let Some(a) = find(inner) {
                    return Some(a);
                }
            }
        }
        o.get("fan_id").map(|v| v.to_string())
    }
    travels.values().filter_map(object_of).find_map(|o| find(&o))
}

/// The OAuth session in what travels, if any (at the top of the main value, or under `session`).
fn session_of(b: &Bridge, travels: &Map<String, Value>) -> Option<Map<String, Value>> {
    let obj = object_of(travels.get(b.carry[0].key)?)?;
    match obj.get("session") {
        Some(Value::Object(o)) if o.contains_key("access_token") => Some(o.clone()),
        _ => obj.contains_key("access_token").then_some(obj),
    }
}

/// The vault sign-in for what travels. It starts from the vault's current copy (`base`), so the
/// members other clients put in (`label`, `way`, `scopes`, `settings`, unknown ones) are kept, and
/// sets the node's copy (`node.settings`), the app it was made with, and the standard members
/// where it is an OAuth session.
pub fn sign_in_for(
    b: &Bridge,
    travels: &Map<String, Value>,
    base: Option<&SignIn>,
    app: Option<String>,
    lineage: &str,
) -> SignIn {
    let mut s: SignIn = match base {
        Some(o) => o.clone(),
        None => serde_json::from_value(json!({ "service": b.service })).expect("a minimal sign-in"),
    };
    s.service = b.service.to_string();
    s.extra
        .insert("node".into(), json!({ "settings": travels, "lineage": lineage }));
    if b.app.is_some() {
        s.client_id = app;
    }
    let sess = session_of(b, travels);
    let mut tokens = Map::new();
    if let Some(sess) = &sess {
        if let Some(at) = sess.get("access_token").and_then(Value::as_str) {
            tokens.insert("access_token".into(), json!(at));
            tokens.insert("token_type".into(), json!("bearer"));
            if let Some(rt) = sess.get("refresh_token").and_then(Value::as_str) {
                tokens.insert("refresh_token".into(), json!(rt));
            }
        }
    }
    s.tokens = tokens;
    s.expires_at = sess
        .as_ref()
        .and_then(|x| x.get("expires_at"))
        .and_then(Value::as_i64)
        .and_then(|e| u64::try_from(e).ok());
    let id = account(travels);
    s.user = id.map(|id| {
        let pick = |keys: &[&str]| {
            keys.iter().find_map(|k| {
                sess.as_ref()
                    .and_then(|x| x.get(*k))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
        };
        ServiceUser {
            id,
            name: pick(&["display_name", "name", "username"]),
            country: pick(&["country", "country_code"]),
            extra: Map::new(),
        }
    });
    // What the standard members were when the node wrote them: a client that writes its own
    // sign-in and keeps `node` (as VAULT.md asks) changes them, so the copy stops counting as the
    // node's
    let std = std_hash(&s);
    if let Some(Value::Object(n)) = s.extra.get_mut("node") {
        n.insert("std".into(), json!(std));
    }
    s
}

/// The standard members a reader without `node` uses: the tokens, their expiry, the account.
fn std_hash(s: &SignIn) -> String {
    let std =
        json!({ "tokens": s.tokens, "expires_at": s.expires_at, "user": s.user.as_ref().map(|u| &u.id) });
    blake3::hash(std.to_string().as_bytes()).to_hex().to_string()
}

/// What a node wrote into a vault sign-in (`None`: written by a client that isn't a node, or
/// rewritten by one since: its standard members aren't the node's any more).
pub fn node_settings(s: &SignIn) -> Option<Map<String, Value>> {
    let node = s.extra.get("node")?;
    if node.get("std").and_then(Value::as_str) != Some(std_hash(s).as_str()) {
        return None;
    }
    match node.get("settings")? {
        Value::Object(o) => Some(o.clone()),
        _ => None,
    }
}

/// Which sign-in a vault copy is, when a node wrote it.
pub fn node_lineage(s: &SignIn) -> Option<String> {
    s.extra.get("node")?.get("lineage")?.as_str().map(str::to_string)
}

/// A hash of a vault copy: of what travels when a node wrote it, else of its standard members.
fn vault_hash(s: &SignIn) -> String {
    match node_settings(s) {
        Some(m) => hash(&m),
        None => format!("std:{}", std_hash(s)),
    }
}

// ---- deciding ----

/// One side of a service, as [`decide`] sees it.
#[derive(Debug, Clone, Default)]
pub struct Side {
    pub hash: String,
    pub account: Option<String>,
    /// Which sign-in it is ([`new_sign_in`]); `None` for a copy no node wrote.
    pub lineage: Option<String>,
    pub expires_at: Option<u64>,
}

impl Side {
    /// When this sign-in was made, as far as anyone knows (0: unknown).
    fn made_at(&self) -> u64 {
        self.lineage.as_deref().map_or(0, lineage_at)
    }
}

/// What [`decide`] knows about "Sign out everywhere".
#[derive(Debug, Clone, Copy, Default)]
pub struct Wipe {
    /// This pass runs right after this node signed out everywhere: every copy here goes aside.
    pub now: bool,
    /// The device list's `signed_out_everywhere_at` now.
    pub at: Option<u64>,
    /// It differs from the one the record was made with: a sign-out everywhere since then.
    pub since_seen: bool,
}

impl Wipe {
    /// A sign-in here made before the last sign-out everywhere (or not known to be newer).
    fn predates(&self, m: &Side) -> bool {
        self.at.is_some_and(|w| w >= m.made_at())
    }
}

/// The vault's side, with what the node needs to take it.
#[derive(Debug, Clone, Default)]
pub struct VaultSide {
    pub side: Side,
    /// A node wrote it (so the node can take it as it is).
    pub from_a_node: bool,
    /// Made with the app this node has (or no app is involved).
    pub our_app: bool,
}

/// What to do for one service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Do {
    Nothing,
    /// Both the same: remember it.
    InStep,
    /// Write the node's copy (first time: the migration).
    Push,
    /// Take the vault's copy.
    Pull,
    /// Removed from the vault elsewhere: move the node's copy aside.
    MoveAside,
    /// Signed out on the node: remove it from the vault.
    Remove,
    /// Nothing on either side any more: forget the record.
    Forget,
    Conflict,
    NotUsable,
}

/// Which way a conflict is settled (`POST /v1/vault/sync {service, take}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Take {
    Node,
    Vault,
}

/// Whether a side is the sign-in `seen` was: by its id where both have one, else by account.
fn same_sign_in(side: &Side, seen: &Synced) -> bool {
    match (&side.lineage, &seen.lineage) {
        (Some(a), Some(b)) => a == b,
        _ => side.account == seen.account,
    }
}

/// The sync rules for one service (module docs). `seen` is what both sides held at the last
/// sync; `wipe` what is known about the person signing out everywhere.
pub fn decide(seen: Option<&Synced>, mine: Option<&Side>, theirs: Option<&VaultSide>, wipe: &Wipe) -> Do {
    let pull = |t: &VaultSide| {
        if t.from_a_node && t.our_app {
            Do::Pull
        } else {
            Do::NotUsable
        }
    };
    // A copy another kind of client wrote, or one made with another app, is written over only by
    // a new sign-in here, and never for another account
    let push = |m: &Side, t: Option<&VaultSide>, new_here: bool| match t {
        Some(t) if !t.from_a_node && t.side.account.is_some() && t.side.account != m.account => Do::Conflict,
        Some(t) if (!t.from_a_node || !t.our_app) && !new_here => Do::NotUsable,
        _ => Do::Push,
    };
    // Both copies are the same account's and both moved on: the one that lasts longer
    let longer = |m: &Side, t: &VaultSide, new_here: bool| {
        if m.expires_at > t.side.expires_at {
            push(m, Some(t), new_here)
        } else {
            pull(t)
        }
    };
    // A conflict record from before a sign-out everywhere says nothing about a sign-in made after
    // it: that one is new here
    let seen = match (seen, mine) {
        (Some(s), Some(m)) if s.conflict && wipe.since_seen && m.lineage != s.lineage => None,
        _ => seen,
    };
    // A copy here that predates the last sign-out everywhere, and isn't the sign-in this node
    // synced since, never goes back in: it gives way to the vault's, or goes aside
    if let Some(m) = mine {
        let confirmed = seen.is_some_and(|s| !s.conflict && same_sign_in(m, s));
        // This very copy was judged after the last sign-out everywhere already (a conflict
        // recorded since): it survived that, whatever its age
        let judged = seen.is_some_and(|s| !wipe.since_seen && s.hash == m.hash);
        if !confirmed && !judged && wipe.predates(m) {
            return match theirs {
                Some(t) if pull(t) == Do::Pull => Do::Pull,
                _ => Do::MoveAside,
            };
        }
    }
    match (mine, theirs) {
        (None, None) => {
            if seen.is_some() {
                Do::Forget
            } else {
                Do::Nothing
            }
        }
        // Signed out everywhere on this node just now: nothing here goes back in
        (Some(_), None) if wipe.now => Do::MoveAside,
        (Some(m), None) => match seen {
            // Never synced: it moves in, unless it is older than the last sign-out everywhere
            None if wipe.predates(m) => Do::MoveAside,
            None => Do::Push,
            // Never in the vault (a conflict): gone from it means signed out everywhere, or only
            // the other sign-in went, and then this one moves in
            Some(s) if s.conflict => {
                if wipe.since_seen {
                    Do::MoveAside
                } else {
                    Do::Push
                }
            }
            // Unchanged here and gone from the vault: signed out elsewhere
            Some(s) if m.hash == s.hash => Do::MoveAside,
            // A new sign-in here goes out, if made after the last sign-out everywhere; a refresh
            // of the old one doesn't bring back what was signed out
            Some(s) if !same_sign_in(m, s) && !wipe.predates(m) => Do::Push,
            Some(_) => Do::MoveAside,
        },
        (None, Some(t)) => match seen {
            None => pull(t),
            Some(s) if s.conflict => pull(t),
            // Unchanged in the vault and gone here: signed out here
            Some(s) if t.side.hash == s.hash => Do::Remove,
            // Another sign-in made elsewhere is taken; a refresh of the old one doesn't undo the
            // sign-out
            Some(s) if !same_sign_in(&t.side, s) => pull(t),
            Some(_) => Do::Remove,
        },
        (Some(m), Some(t)) => {
            if m.hash == t.side.hash {
                return Do::InStep;
            }
            // A conflict when the person signed out everywhere since: this node's side goes too
            // (the next pass takes whatever the vault has now)
            if seen.is_some_and(|s| s.conflict) && wipe.since_seen {
                return Do::MoveAside;
            }
            let Some(s) = seen.filter(|s| !s.conflict) else {
                // First contact with two different copies (or still that conflict): the same
                // account is one sign-in refreshed on two sides; another account is a conflict
                if m.account.is_some() && m.account == t.side.account {
                    // A copy here from before the last sign-out everywhere doesn't win
                    if wipe.predates(m) {
                        return pull(t);
                    }
                    return longer(m, t, false);
                }
                return Do::Conflict;
            };
            let here = m.hash != s.hash;
            let there = t.side.hash != s.hash;
            let new_here = !same_sign_in(m, s);
            let new_there = !same_sign_in(&t.side, s);
            match (here, there) {
                (false, false) => Do::InStep,
                (true, false) => push(m, Some(t), new_here),
                (false, true) => pull(t),
                (true, true) => match (new_here, new_there) {
                    (true, false) => push(m, Some(t), true),
                    (false, true) => pull(t),
                    (true, true) => Do::Conflict,
                    (false, false) => longer(m, t, false),
                },
            }
        }
    }
}

// ---- doing ----

fn remember(lib: &Library, service: &str, synced: &Synced) {
    let _ = lib.set_setting(
        &synced_key(service),
        &serde_json::to_string(synced).unwrap_or_default(),
    );
}

/// The record for a service, if it was made with this vault (`owner`) by this machine (`client`).
/// A record that can't be read is an error (taken as "never synced", a local sign-out would be
/// undone); one that doesn't parse is from an older build and counts as none.
fn last_synced(lib: &Library, service: &str, owner: &str, client: &str) -> Result<Option<Synced>, String> {
    Ok(lib
        .get_setting(&synced_key(service))
        .map_err(|e| e.to_string())?
        .and_then(|v| serde_json::from_str::<Synced>(&v).ok())
        .filter(|s| s.owner == owner && s.client == client))
}

fn side_of(b: &Bridge, travels: &Map<String, Value>, lineage: Option<String>) -> Side {
    Side {
        hash: hash(travels),
        account: account(travels),
        lineage,
        expires_at: session_of(b, travels)
            .and_then(|x| x.get("expires_at").and_then(Value::as_i64))
            .and_then(|e| u64::try_from(e).ok()),
    }
}

/// The app a node copy's tokens belong to: the node's, except an imported session's (it belongs
/// to the app it was imported from).
fn app_for(b: &Bridge, travels: Option<&Map<String, Value>>, node_app: Option<String>) -> Option<String> {
    let imported = travels
        .and_then(|m| session_of(b, m))
        .is_some_and(|x| x.get("from").is_some_and(|f| !f.is_null()));
    if imported {
        None
    } else {
        node_app
    }
}

/// One pass over every bridge, waiting for any other pass (or "Sign out everywhere") to end.
pub async fn sync(s: &NodeState, vault: &Vault<'_>) -> Result<CacheReport, VaultStoreError> {
    let _pass = s.vault_pass.lock().await;
    sync_held(s, vault, None, false).await
}

/// [`sync`] with [`NodeState::vault_pass`] already held. `take` settles one service's conflict
/// (or takes a copy that isn't usable as it is) the way it says. `wiping`: this node just signed
/// out everywhere, so every copy here goes aside.
pub(crate) async fn sync_held(
    s: &NodeState,
    vault: &Vault<'_>,
    take: Option<(&str, Take)>,
    wiping: bool,
) -> Result<CacheReport, VaultStoreError> {
    let mut report = CacheReport::default();
    let owner = vault.owner().to_string();
    let client = vault.client_key().to_hex();
    // Whether the person signed out everywhere decides too much to guess: without the device
    // list, nothing is synced this time
    let marker = match vault.index().await {
        Ok(ix) => Vault::signed_out_everywhere_at(&ix),
        Err(e) => {
            tracing::warn!("vault: the device list can't be read, not synced this time: {e}");
            report.failed = BRIDGES.iter().map(|b| b.service.to_string()).collect();
            return Ok(report);
        }
    };
    for b in BRIDGES {
        let service = b.service.to_string();
        let theirs: Option<Opened> = match vault.read(b.service).await {
            Ok(o) => o,
            Err(VaultStoreError::Vault(VaultError::NotARecipient)) => {
                report.not_readable.push(service);
                continue;
            }
            Err(e) => {
                tracing::warn!(service = b.service, "vault: not synced this time: {e}");
                report.failed.push(service);
                continue;
            }
        };
        let node_app = b.app.and_then(|f| f(s));
        let (mine, seen, lineage) = {
            let lib = s.library.lock().expect("library poisoned");
            let mine = match carried(&lib, b) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        service = b.service,
                        "vault: the node's setting can't be read: {e}"
                    );
                    report.failed.push(service);
                    continue;
                }
            };
            let lineage = match mine.as_ref().map(|_| local_lineage(&lib, b.service)).transpose() {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(
                        service = b.service,
                        "vault: the node's sign-in id can't be read: {e}"
                    );
                    report.failed.push(service);
                    continue;
                }
            };
            let seen = match last_synced(&lib, b.service, &owner, &client) {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(service = b.service, "vault: the sync record can't be read: {e}");
                    report.failed.push(service);
                    continue;
                }
            };
            (mine, seen, lineage)
        };
        let app = app_for(b, mine.as_ref(), node_app.clone());
        let my_side = mine.as_ref().map(|m| side_of(b, m, lineage.clone()));
        let their_side = theirs.as_ref().map(|o| {
            let from_a_node = node_settings(&o.sign_in);
            VaultSide {
                side: match &from_a_node {
                    Some(m) => side_of(b, m, node_lineage(&o.sign_in)),
                    None => Side {
                        hash: vault_hash(&o.sign_in),
                        account: o.sign_in.user.as_ref().map(|u| u.id.clone()),
                        lineage: None,
                        expires_at: o.sign_in.expires_at,
                    },
                },
                from_a_node: from_a_node.is_some(),
                our_app: b.app.is_none() || o.sign_in.client_id.is_none() || o.sign_in.client_id == node_app,
            }
        });
        let wipe = Wipe {
            now: wiping,
            at: marker,
            since_seen: marker.is_some() && seen.as_ref().is_some_and(|x| x.marker != marker),
        };
        let mut what = decide(seen.as_ref(), my_side.as_ref(), their_side.as_ref(), &wipe);
        // Lost here because the node's app changed, not signed out: the vault keeps it
        if what == Do::Remove && seen.as_ref().is_some_and(|x| x.app != node_app) {
            what = Do::Forget;
        }
        // The person settles it
        if let Some((svc, how)) = take {
            if svc == b.service && matches!(what, Do::Conflict | Do::NotUsable) {
                what = match how {
                    Take::Node if mine.is_some() => Do::Push,
                    Take::Vault if their_side.as_ref().is_some_and(|t| t.from_a_node) => Do::Pull,
                    _ => what,
                };
            }
        }
        let record = |lib: &Library,
                      revision: u64,
                      travels: &Map<String, Value>,
                      lineage: Option<String>,
                      conflict: bool| {
            remember(
                lib,
                b.service,
                &Synced {
                    revision,
                    hash: hash(travels),
                    account: account(travels),
                    owner: owner.clone(),
                    conflict,
                    lineage,
                    client: client.clone(),
                    app: node_app.clone(),
                    at: pm_library::db::now_unix().max(0) as u64,
                    marker,
                },
            )
        };
        // Written only while the vault holds what this pass decided on
        let expected = theirs.as_ref().map(|o| vault_hash(&o.sign_in));
        let unchanged = |cur: Option<&SignIn>| cur.map(vault_hash) == expected;
        match what {
            Do::Nothing => {}
            Do::InStep => {
                if let (Some(m), Some(o)) = (&mine, &theirs) {
                    let lib = s.library.lock().expect("library poisoned");
                    // Both sides are one sign-in now: the node takes the vault's id for it
                    let their_lineage = node_lineage(&o.sign_in);
                    if let Some(l) = &their_lineage {
                        if lineage.as_ref() != Some(l) {
                            let _ = lib.set_setting(&lineage_key(b.service), l);
                        }
                    }
                    let lin = their_lineage.or(lineage.clone());
                    if seen.as_ref().is_none_or(|x| {
                        x.conflict
                            || x.hash != hash(m)
                            || x.revision != o.sign_in.revision
                            || x.lineage != lin
                            || x.marker != marker
                    }) {
                        record(&lib, o.sign_in.revision, m, lin, false);
                    }
                }
            }
            Do::Push => {
                let m = mine.as_ref().expect("push has a node copy");
                let lin = lineage.clone().unwrap_or_default();
                let si = sign_in_for(b, m, theirs.as_ref().map(|o| &o.sign_in), app.clone(), &lin);
                match vault
                    .write_shared_if(b.service, &si, Some(marker), &unchanged)
                    .await
                {
                    Ok(Some(revision)) => {
                        record(
                            &s.library.lock().expect("library poisoned"),
                            revision,
                            m,
                            lineage.clone(),
                            false,
                        );
                        if seen.is_none() && theirs.is_none() {
                            report.migrated.push(service);
                        } else {
                            report.pushed.push(service);
                        }
                    }
                    Ok(None) => report.failed.push(service),
                    Err(e) => {
                        tracing::warn!(service = b.service, "vault: not written this time: {e}");
                        report.failed.push(service);
                    }
                }
            }
            Do::Pull => {
                let o = theirs.as_ref().expect("pull has a vault copy");
                let travels = node_settings(&o.sign_in).unwrap_or_default();
                let lib = s.library.lock().expect("library poisoned");
                // A source that changed its sign-in since this pass read it wins: next pass
                if carried(&lib, b).ok().flatten().map(|m| hash(&m)) != mine.as_ref().map(hash) {
                    continue;
                }
                apply(&lib, b, &travels);
                reset_after_pull(&lib, b);
                let lin = node_lineage(&o.sign_in);
                match &lin {
                    Some(l) => {
                        let _ = lib.set_setting(&lineage_key(b.service), l);
                    }
                    None => {
                        let _ = lib.delete_setting(&lineage_key(b.service));
                    }
                }
                record(&lib, o.sign_in.revision, &travels, lin, false);
                report.pulled.push(service);
            }
            Do::MoveAside => {
                let lib = s.library.lock().expect("library poisoned");
                if carried(&lib, b).ok().flatten().map(|m| hash(&m)) != mine.as_ref().map(hash) {
                    continue;
                }
                move_aside(&lib, b);
                let _ = lib.delete_setting(&synced_key(b.service));
                report.signed_out.push(service);
            }
            Do::Remove => match vault.remove_if(b.service, &unchanged).await {
                Ok(true) => {
                    let lib = s.library.lock().expect("library poisoned");
                    let _ = lib.delete_setting(&synced_key(b.service));
                    let _ = lib.delete_setting(&lineage_key(b.service));
                    report.removed.push(service);
                }
                Ok(false) => report.failed.push(service),
                Err(e) => {
                    tracing::warn!(service = b.service, "vault: not removed this time: {e}");
                    report.failed.push(service);
                }
            },
            Do::Forget => {
                let lib = s.library.lock().expect("library poisoned");
                let _ = lib.delete_setting(&synced_key(b.service));
                if mine.is_none() && theirs.is_some() {
                    report.forgotten.push(service);
                }
            }
            Do::Conflict => {
                if let (Some(m), Some(o)) = (&mine, &theirs) {
                    let lib = s.library.lock().expect("library poisoned");
                    if seen
                        .as_ref()
                        .is_none_or(|x| !x.conflict || x.hash != hash(m) || x.marker != marker)
                    {
                        record(&lib, o.sign_in.revision, m, lineage.clone(), true);
                    }
                }
                report.conflicts.push(service)
            }
            Do::NotUsable => report.not_usable.push(service),
        }
    }
    Ok(report)
}

/// How often the node syncs with the vault while it is signed in.
pub const EVERY: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// After a sign-in or a restored session: list the node in the device list, share with devices
/// that joined, sync the sign-ins; then again every [`EVERY`] while this session is the node's.
pub fn start(s: &Arc<NodeState>, sync: Arc<crate::sync::PubkySync>) {
    let s = s.clone();
    tokio::spawn(async move {
        // The settings mirror on the homeserver (`settings.json`) once held TIDAL's sign-in in
        // plain text; writing it again now leaves it out
        sync.backup_settings(&s).await;
        loop {
            if !s.sync_handle().is_some_and(|h| Arc::ptr_eq(&h, &sync)) {
                break;
            }
            match crate::vault::client_secret(&s) {
                Ok(secret) => {
                    let revoked = crate::vault::revoked_file(&s);
                    let vault = Vault::new(sync.session(), &secret)
                        .device("node")
                        .remember_revocations(&revoked);
                    match pass(&s, &vault, None).await {
                        Ok(r) if r != CacheReport::default() => tracing::info!(?r, "vault: sign-ins synced"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("vault: {e}"),
                    }
                }
                Err(e) => tracing::warn!("vault: {}", e.message),
            }
            tokio::time::sleep(EVERY).await;
        }
    });
}

/// Where the last pass's report is kept (`GET /v1/vault` shows it).
pub const LAST_SYNC_KEY: &str = "vault.last_sync";

/// Register, share, sync: one round of [`start`], one at a time. The report is kept for Settings.
pub async fn pass(
    s: &NodeState,
    vault: &Vault<'_>,
    take: Option<(&str, Take)>,
) -> Result<CacheReport, VaultStoreError> {
    let _pass = s.vault_pass.lock().await;
    vault.touch(&s.name).await?;
    vault.share_with_trusted().await?;
    let r = sync_held(s, vault, take, false).await?;
    let lib = s.library.lock().expect("library poisoned");
    let _ = lib.set_setting(
        LAST_SYNC_KEY,
        &json!({ "at": pm_library::db::now_unix(), "report": &r }).to_string(),
    );
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge(service: &str) -> &'static Bridge {
        BRIDGES.iter().find(|b| b.service == service).unwrap()
    }

    fn lib() -> Library {
        Library::open_in_memory().unwrap()
    }

    fn side(hash: &str, account: &str) -> Side {
        Side {
            hash: hash.into(),
            account: Some(account.into()),
            lineage: None,
            expires_at: None,
        }
    }

    fn vside(hash: &str, account: &str) -> VaultSide {
        VaultSide {
            side: side(hash, account),
            from_a_node: true,
            our_app: true,
        }
    }

    fn seen(hash: &str, account: &str) -> Synced {
        Synced {
            revision: 1,
            hash: hash.into(),
            account: Some(account.into()),
            owner: "o".into(),
            ..Default::default()
        }
    }

    fn lin(mut x: Side, l: &str) -> Side {
        x.lineage = Some(l.into());
        x
    }

    fn vlin(mut x: VaultSide, l: &str) -> VaultSide {
        x.side.lineage = Some(l.into());
        x
    }

    #[test]
    fn the_sync_rules() {
        let d = |seen: Option<&Synced>, m: Option<&Side>, t: Option<&VaultSide>| {
            decide(seen, m, t, &Wipe::default())
        };
        let s = seen("h1", "7");
        // First time: migrate, take; two copies of one account are one sign-in (the longer-lived
        // wins); two accounts are a conflict
        assert_eq!(d(None, Some(&side("a", "7")), None), Do::Push);
        assert_eq!(d(None, None, Some(&vside("a", "7"))), Do::Pull);
        assert_eq!(d(None, Some(&side("a", "7")), Some(&vside("a", "7"))), Do::InStep);
        assert_eq!(d(None, Some(&side("a", "7")), Some(&vside("b", "7"))), Do::Pull);
        let mut lasting = side("a", "7");
        lasting.expires_at = Some(9);
        assert_eq!(d(None, Some(&lasting), Some(&vside("b", "7"))), Do::Push);
        assert_eq!(
            d(None, Some(&side("a", "7")), Some(&vside("b", "9"))),
            Do::Conflict,
            "two nodes with their own sign-ins: neither replaces the other"
        );
        // A conflict stays one until the two agree
        let mut c = seen("a", "7");
        c.conflict = true;
        assert_eq!(
            d(Some(&c), Some(&side("a", "7")), Some(&vside("b2", "9"))),
            Do::Conflict
        );
        assert_eq!(
            d(Some(&c), Some(&side("b2", "9")), Some(&vside("b2", "9"))),
            Do::InStep
        );
        // ...and when the vault's copy goes: signed out everywhere moves ours aside, the other
        // sign-in alone going lets ours in
        assert_eq!(
            decide(
                Some(&c),
                Some(&side("a", "7")),
                None,
                &Wipe {
                    now: false,
                    at: Some(5),
                    since_seen: true
                }
            ),
            Do::MoveAside
        );
        assert_eq!(
            decide(Some(&c), Some(&side("a", "7")), None, &Wipe::default()),
            Do::Push
        );
        // Unchanged on both sides, even when the vault's revision moved (a sign-out rotation)
        assert_eq!(
            d(Some(&s), Some(&side("h1", "7")), Some(&vside("h1", "7"))),
            Do::InStep
        );
        // One side changed: it wins
        assert_eq!(
            d(Some(&s), Some(&side("h2", "7")), Some(&vside("h1", "7"))),
            Do::Push
        );
        assert_eq!(
            d(Some(&s), Some(&side("h1", "7")), Some(&vside("h2", "7"))),
            Do::Pull
        );
        // A blob made again after Sign out everywhere (revision 1 again) is a change, by content
        assert_eq!(
            d(Some(&s), Some(&side("h1", "7")), Some(&vside("new", "9"))),
            Do::Pull
        );
        // Signed out on this node: removed from the vault, not pulled back
        assert_eq!(d(Some(&s), None, Some(&vside("h1", "7"))), Do::Remove);
        assert_eq!(
            d(Some(&s), None, Some(&vside("h2", "7"))),
            Do::Remove,
            "a refresh elsewhere"
        );
        assert_eq!(
            d(Some(&s), None, Some(&vside("h2", "9"))),
            Do::Pull,
            "a new account elsewhere"
        );
        // Removed from the vault elsewhere: aside here, unless a new sign-in was made here
        assert_eq!(d(Some(&s), Some(&side("h1", "7")), None), Do::MoveAside);
        assert_eq!(
            d(Some(&s), Some(&side("h2", "7")), None),
            Do::MoveAside,
            "a refresh here"
        );
        assert_eq!(
            d(Some(&s), Some(&side("h2", "9")), None),
            Do::Push,
            "a new account here"
        );
        assert_eq!(d(Some(&s), None, None), Do::Forget);
        // Both changed: the new sign-in wins; the same one, the longer-lived copy
        assert_eq!(
            d(Some(&s), Some(&side("x", "9")), Some(&vside("y", "7"))),
            Do::Push
        );
        assert_eq!(
            d(Some(&s), Some(&side("x", "7")), Some(&vside("y", "9"))),
            Do::Pull
        );
        assert_eq!(
            d(Some(&s), Some(&side("x", "8")), Some(&vside("y", "9"))),
            Do::Conflict
        );
        let mut later = side("x", "7");
        later.expires_at = Some(200);
        let mut earlier = vside("y", "7");
        earlier.side.expires_at = Some(100);
        assert_eq!(d(Some(&s), Some(&later), Some(&earlier)), Do::Push);
        earlier.side.expires_at = Some(300);
        assert_eq!(d(Some(&s), Some(&later), Some(&earlier)), Do::Pull);
        // Another app's tokens, or a copy no node wrote: not taken, and not written over by a
        // refresh here
        let mut other_app = vside("h2", "7");
        other_app.our_app = false;
        assert_eq!(
            d(Some(&s), Some(&side("h1", "7")), Some(&other_app)),
            Do::NotUsable
        );
        let mut phone = vside("p", "9");
        phone.from_a_node = false;
        assert_eq!(d(None, None, Some(&phone)), Do::NotUsable);
        assert_eq!(d(Some(&s), Some(&side("h2", "7")), Some(&phone)), Do::NotUsable);
        let mut phone7 = vside("h1", "7");
        phone7.from_a_node = false;
        assert_eq!(
            d(Some(&s), Some(&side("h2", "7")), Some(&phone7)),
            Do::NotUsable,
            "a phone's copy of the same account isn't overwritten by a refresh"
        );
        let mut phone7b = vside("p7", "7");
        phone7b.from_a_node = false;
        assert_eq!(d(Some(&s), Some(&side("x", "9")), Some(&phone7b)), Do::Conflict);
    }

    #[test]
    fn a_new_sign_in_is_told_from_a_refresh_by_its_id() {
        let d = |seen: Option<&Synced>, m: Option<&Side>, t: Option<&VaultSide>| {
            decide(seen, m, t, &Wipe::default())
        };
        let mut s = seen("h1", "7");
        s.lineage = Some("L1".into());
        // Signed out everywhere elsewhere, then signed in again here to the same account: it
        // goes out (a refresh of L1 would be moved aside)
        assert_eq!(d(Some(&s), Some(&lin(side("h2", "7"), "L2")), None), Do::Push);
        assert_eq!(
            d(Some(&s), Some(&lin(side("h2", "7"), "L1")), None),
            Do::MoveAside
        );
        // Signed out here, and another node signed in again to the same account: taken
        assert_eq!(d(Some(&s), None, Some(&vlin(vside("h2", "7"), "L2"))), Do::Pull);
        assert_eq!(d(Some(&s), None, Some(&vlin(vside("h2", "7"), "L1"))), Do::Remove);
        // Signed out everywhere and in again elsewhere, while this node refreshed the old chain:
        // the new sign-in wins even though the old copy lasts longer
        let mut old = lin(side("x", "7"), "L1");
        old.expires_at = Some(500);
        let mut new = vlin(vside("y", "7"), "L2");
        new.side.expires_at = Some(100);
        assert_eq!(d(Some(&s), Some(&old), Some(&new)), Do::Pull);
        // A sign-in with no account member at all (Jamendo's has only a name) by its id too
        let mut j = seen("h1", "");
        j.account = None;
        j.lineage = Some("L1".into());
        let mut mine = lin(side("h2", ""), "L2");
        mine.account = None;
        assert_eq!(d(Some(&j), Some(&mine), None), Do::Push);
    }

    #[test]
    fn signing_out_everywhere_is_never_undone() {
        let none = Wipe::default();
        let w = |since_seen| Wipe {
            now: false,
            at: Some(100),
            since_seen,
        };
        // Right after this node signed out everywhere: everything here goes aside, a sign-in
        // made a minute ago (not synced yet) too
        let now = Wipe {
            now: true,
            at: Some(100),
            since_seen: true,
        };
        assert_eq!(
            decide(None, Some(&lin(side("a", "7"), "99-x")), None, &now),
            Do::MoveAside
        );
        assert_eq!(
            decide(None, Some(&lin(side("a", "7"), "99-x")), None, &none),
            Do::Push
        );
        // Elsewhere: a copy never synced, or a new sign-in, older than the sign-out goes aside;
        // one made after it moves in
        assert_eq!(
            decide(None, Some(&lin(side("a", "7"), "50-x")), None, &w(false)),
            Do::MoveAside
        );
        assert_eq!(
            decide(None, Some(&lin(side("a", "7"), "0-x")), None, &w(false)),
            Do::MoveAside
        );
        assert_eq!(
            decide(None, Some(&lin(side("a", "7"), "150-x")), None, &w(false)),
            Do::Push
        );
        let mut s = seen("h1", "7");
        s.lineage = Some("10-L1".into());
        assert_eq!(
            decide(Some(&s), Some(&lin(side("h2", "7"), "50-L2")), None, &w(true)),
            Do::MoveAside
        );
        assert_eq!(
            decide(Some(&s), Some(&lin(side("h2", "7"), "150-L2")), None, &w(true)),
            Do::Push
        );
        // A conflict whose vault side was filled again after a sign-out everywhere: this side,
        // from before it, gives way to the new sign-in
        let mut c = seen("a", "9");
        c.conflict = true;
        assert_eq!(
            decide(Some(&c), Some(&side("a", "9")), Some(&vside("b", "7")), &w(true)),
            Do::Pull
        );
        assert_eq!(
            decide(Some(&c), Some(&side("a", "9")), Some(&vside("b", "7")), &w(false)),
            Do::Conflict
        );
        // First contact, same account, this copy from before the sign-out: the vault's wins
        let mut lasting = lin(side("a", "7"), "50-x");
        lasting.expires_at = Some(9_999);
        assert_eq!(
            decide(None, Some(&lasting), Some(&vside("b", "7")), &w(false)),
            Do::Pull
        );
        // A sign-in made here before the wipe and not synced, meeting a copy written after it: a
        // phone's (not usable here) puts it aside, another node's is taken; never pushed
        let mut phone = vside("p", "8");
        phone.from_a_node = false;
        assert_eq!(
            decide(
                Some(&s),
                Some(&lin(side("h2", "8"), "50-L2")),
                Some(&phone),
                &w(true)
            ),
            Do::MoveAside
        );
        let other = vlin(vside("c", "9"), "160-L3");
        assert_eq!(
            decide(
                Some(&s),
                Some(&lin(side("h2", "8"), "50-L2")),
                Some(&other),
                &w(true)
            ),
            Do::Pull
        );
        // A node in a conflict that signs in again after the wipe: a new sign-in, not moved aside
        let mut cf = seen("a", "9");
        cf.conflict = true;
        cf.lineage = Some("1-L0".into());
        assert_eq!(
            decide(Some(&cf), Some(&lin(side("n", "9"), "150-L5")), None, &w(true)),
            Do::Push
        );
        assert_eq!(
            decide(
                Some(&cf),
                Some(&lin(side("n", "9"), "150-L5")),
                Some(&vlin(vside("c", "7"), "160-L3")),
                &w(true)
            ),
            Do::Conflict
        );
        assert_eq!(lineage_at("150-abc"), 150);
        assert_eq!(lineage_at("abc"), 0);
    }

    #[test]
    fn a_phone_that_keeps_the_node_member_is_not_taken_for_the_node() {
        let l = lib();
        let b = bridge("spotify");
        l.set_setting(
            "spotify.user",
            r#"{"access_token":"at","refresh_token":"rt1","expires_at":100,"user_id":"gil"}"#,
        )
        .unwrap();
        let m = carried(&l, b).unwrap().unwrap();
        let ours = sign_in_for(b, &m, None, None, "1-L");
        assert_eq!(node_settings(&ours).unwrap(), m);
        assert!(!vault_hash(&ours).starts_with("std:"));
        // The phone signs in again and rewrites the blob, keeping `node` as the spec asks
        let mut phone = ours.clone();
        phone.tokens.insert("refresh_token".into(), json!("phone-rt"));
        assert!(node_settings(&phone).is_none(), "a copy the node didn't write");
        assert!(vault_hash(&phone).starts_with("std:"));
        assert_ne!(vault_hash(&phone), vault_hash(&ours));
    }

    #[test]
    fn a_pulled_last_fm_session_drops_the_old_ones_problem() {
        let l = lib();
        let b = bridge("lastfm");
        l.set_setting(
            "lastfm.session",
            r#"{"key":"old","user":"gil","submit":false,"problem":"error 9"}"#,
        )
        .unwrap();
        let mut travels = Map::new();
        travels.insert("lastfm.session".into(), json!({"key": "new", "user": "gil"}));
        apply(&l, b, &travels);
        reset_after_pull(&l, b);
        let v: Value = serde_json::from_str(&l.get_setting("lastfm.session").unwrap().unwrap()).unwrap();
        assert_eq!(v, json!({"key": "new", "user": "gil", "submit": false}));
    }

    #[test]
    fn only_the_sign_in_travels() {
        let l = lib();
        let b = bridge("tidal-full");
        l.set_setting(
            "tidal.unofficial",
            r#"{"enabled":true,"quality":"max","session":{"access_token":"a2","refresh_token":"r2","expires_at":5,"user_id":"42"},"health":{"ok":true,"detail":"","at":9}}"#,
        )
        .unwrap();
        let m = carried(&l, b).unwrap().unwrap();
        assert_eq!(
            m["tidal.unofficial"],
            json!({"session": {"access_token":"a2","refresh_token":"r2","expires_at":5,"user_id":"42"}})
        );
        assert_eq!(account(&m).as_deref(), Some("42"));
        // Health changing doesn't change what travels
        l.set_setting(
            "tidal.unofficial",
            r#"{"enabled":true,"quality":"max","session":{"access_token":"a2","refresh_token":"r2","expires_at":5,"user_id":"42"},"health":{"ok":false,"detail":"x","at":10}}"#,
        )
        .unwrap();
        assert_eq!(hash(&carried(&l, b).unwrap().unwrap()), hash(&m));
        // Another node takes the session and keeps its own switch and health
        let other = lib();
        other
            .set_setting(
                "tidal.unofficial",
                r#"{"enabled":false,"quality":"high","health":{"ok":false,"detail":"y","at":1}}"#,
            )
            .unwrap();
        assert!(carried(&other, b).unwrap().is_none(), "no session: not signed in");
        apply(&other, b, &m);
        let v: Value =
            serde_json::from_str(&other.get_setting("tidal.unofficial").unwrap().unwrap()).unwrap();
        assert_eq!(v["enabled"], false);
        assert_eq!(v["quality"], "high");
        assert_eq!(v["health"]["detail"], "y");
        assert_eq!(v["session"]["access_token"], "a2");
        // Moved aside: the session goes, the rest stays
        move_aside(&other, b);
        let v: Value =
            serde_json::from_str(&other.get_setting("tidal.unofficial").unwrap().unwrap()).unwrap();
        assert!(v.get("session").is_none());
        assert_eq!(v["quality"], "high");
        assert!(other
            .get_setting("vault.signed_out.tidal.unofficial")
            .unwrap()
            .unwrap()
            .contains("a2"));
        assert!(carried(&other, b).unwrap().is_none());
    }

    #[test]
    fn bandcamp_carries_its_cookie_and_signs_out_of_it() {
        let l = lib();
        let b = bridge("bandcamp");
        l.set_setting(
            "bandcamp.unofficial",
            r#"{"enabled":true,"fan":{"fan_id":5},"format":"flac"}"#,
        )
        .unwrap();
        assert!(carried(&l, b).unwrap().is_none(), "no cookie: not signed in");
        l.set_setting("bandcamp.identity", "cookie-1").unwrap();
        let m = carried(&l, b).unwrap().unwrap();
        assert_eq!(m["bandcamp.identity"], "cookie-1");
        assert_eq!(account(&m).as_deref(), Some("5"));
        // A copy without the cookie (signed out elsewhere) removes it here
        let mut gone = m.clone();
        gone.remove("bandcamp.identity");
        apply(&l, b, &gone);
        assert_eq!(l.get_setting("bandcamp.identity").unwrap(), None);
    }

    #[test]
    fn the_vault_copy_keeps_what_other_clients_put_in() {
        let l = lib();
        let b = bridge("spotify");
        l.set_setting(
            "spotify.user",
            r#"{"access_token":"at","refresh_token":"RTSECRET7731","expires_at":1761403600,"user_id":"gil","display_name":"Gil","country":"CH"}"#,
        )
        .unwrap();
        let m = carried(&l, b).unwrap().unwrap();
        let mut base: SignIn = serde_json::from_value(json!({
            "service": "spotify", "label": "gil@example.org", "way": "app", "scopes": ["streaming"],
            "settings": {"quality": "max"}, "future_member": 1
        }))
        .unwrap();
        base.revision = 4;
        let s = sign_in_for(b, &m, Some(&base), Some("app-1".into()), "L1");
        assert_eq!(s.label.as_deref(), Some("gil@example.org"));
        assert_eq!(s.way.as_deref(), Some("app"));
        assert_eq!(s.scopes, ["streaming"]);
        assert_eq!(s.extra["future_member"], 1);
        assert_eq!(s.client_id.as_deref(), Some("app-1"));
        assert_eq!(s.tokens["refresh_token"], "RTSECRET7731");
        assert_eq!(s.expires_at, Some(1761403600));
        let u = s.user.clone().unwrap();
        assert_eq!(
            (u.id.as_str(), u.name.as_deref(), u.country.as_deref()),
            ("gil", Some("Gil"), Some("CH"))
        );
        assert_eq!(node_settings(&s).unwrap(), m);
        assert!(!format!("{s:?}").contains("RTSECRET7731"), "Debug shows a token");
    }

    #[test]
    fn a_bridge_for_a_source_uses_its_source_id() {
        let sources: Vec<&str> = crate::streaming::services().iter().map(|s| s.id()).collect();
        for b in BRIDGES {
            let base = b.service.split('-').next().unwrap_or(b.service);
            let integration = ["lastfm", "listenbrainz"].contains(&b.service);
            assert!(
                integration || sources.contains(&base),
                "{} is not a source id",
                b.service
            );
        }
    }

    #[test]
    fn bridges_are_valid_vault_ids_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for b in BRIDGES {
            assert!(
                pubky_music_specs::vault::is_service_id(b.service),
                "{}",
                b.service
            );
            assert!(seen.insert(b.service), "{} twice", b.service);
            assert!(!b.carry.is_empty());
        }
        // What stays on the machine, and the node's apps (ADR 0016: the admin's, not a person's)
        for local in [
            "spotify.librespot",
            "spotify.soloist",
            "youtube.unofficial",
            "youtube.api_key",
            "spotify.app",
            "qobuz.app",
            "soundcloud.client",
            "lastfm.app",
            "tidal.unofficial_client",
            "plex.client_id",
            "podcasts.gpodder",
            "jellyfin.server",
            "drives.accounts",
        ] {
            assert!(
                BRIDGES.iter().all(|b| b.carry.iter().all(|c| c.key != local)),
                "{local}"
            );
        }
    }
}
