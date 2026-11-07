//! Accounts on the node, per person (STEWARD-BUNDLED-HOMESERVER.md, step 3): the owner and
//! listeners. Everyone signs in with Pubky Ring (`/v1/auth/pubky/start|poll`):
//! - the **owner** (the node's own Pubky identity) gets a browser session of their own (`pmo_…`,
//!   kept only as its hash, ended by Log out); apps and integrations keep using the node's API
//!   token, which a browser may still be given by hand;
//! - a **listener** (someone in People with music access, `people::may_use_music`) gets a
//!   personal token, kept only as its hash. It opens the shared library, search, sources and the
//!   zones the owner allows; settings, People, the homeserver, sources' sign-ins and the
//!   library's folders stay the owner's. Taking music access away (People) stops the token at
//!   once.
//!
//! Each request carries a [`Caller`] (an extension) for handlers that care who is asking.
//! Listeners' own playlists, bookmarks and history (on their own homeserver) come with the
//! node agent's per-person sync; until then a listener's writes to them are refused.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::Json;
use pm_zones::FetchLimit;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const TOKENS: &str = "accounts.tokens";
pub(crate) const OWNER_SESSIONS: &str = "accounts.owner_sessions";
/// The owner's devices (the desktop and Android app), signed in with Pubky Ring: tokens with the
/// same `pmo_` start as browser sessions, kept apart so a browser's Log out and the browsers' cap
/// leave them. Make a new token and a change of the node's identity end them too.
pub(crate) const DEVICE_SESSIONS: &str = "accounts.device_sessions";
/// Browser sessions of the owner kept at once; the oldest goes first.
const OWNER_SESSIONS_CAP: usize = 20;
/// Devices of the owner kept at once (an app signing in again and again, say reinstalled).
const DEVICE_SESSIONS_CAP: usize = 50;
// Room for more devices than browsers: a device signs in once and stays
const _: () = assert!(DEVICE_SESSIONS_CAP > OWNER_SESSIONS_CAP);
/// A hash of the API token the owner's browser sessions were made under: one changed in the config
/// ends them at the next start, as Make a new token does.
/// Never in a backup (`backup::NODE_OWN`).
pub(crate) const OWNER_SESSIONS_TOKEN: &str = "accounts.owner_sessions_token";
/// The start of an owner's browser session token.
pub const OWNER_PREFIX: &str = "pmo_";
const ZONES: &str = "accounts.listener_zones";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Listener,
    /// A party guest (party.rs): adds songs to one party, nothing else.
    Guest,
}

/// Who made a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub role: Role,
    /// Their public key (z32), when known.
    pub key: Option<String>,
}

/// Whose streaming sign-ins a request or a queue item uses (TASK-per-person-service-accounts):
/// each person plays with their own TIDAL, Qobuz, Spotify… account, never anyone else's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Who {
    /// The owner: the node's sign-ins, under the keys they always had (nothing moved).
    Owner,
    /// A listener (their z32 key): their own sign-ins, under `person.<key>.<setting>`.
    Listener(String),
    /// A listener playing through the node's sign-ins (the admin's source policy, [`source_policy`]):
    /// the same person for everything of their own (play order, preferences, who queued what), but
    /// their TIDAL, Qobuz and Spotify are the node's ([`SIGNIN_KEYS`]). Only the paths that *use* an
    /// account get this ([`Who::resolved`]); signing in and out always act on their own.
    ListenerOnNode(String),
    /// No one with sign-ins: a party guest, a LAN controller, an item queued by someone the node
    /// can't name. Nothing that needs a sign-in plays for them.
    Nobody,
}

/// Where listeners' own settings start: `person.<key>.<setting>`.
pub const PERSON_PREFIX: &str = "person.";

/// The node admin's choice of whose streaming sign-ins listeners play with (owner, 2026-11-03: "can we
/// have both and node admin can set if user use node or own sources").
pub const SOURCE_POLICY_KEY: &str = "sources.policy";
/// A listener's own choice under [`SourcePolicy::Choice`]: `node` (the default) or `own`.
pub const SOURCE_USE_KEY: &str = "sources.use";

/// Whose sign-ins listeners play TIDAL, Qobuz and Spotify with. The owner always plays with the
/// node's: theirs are the node's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourcePolicy {
    /// Everyone plays through the node's sign-ins; listeners can't sign in to their own.
    Node,
    /// Each person signs in to their own (how it was since per-person accounts, the default).
    #[default]
    Own,
    /// The node's by default; each listener may switch to their own.
    Choice,
}

impl SourcePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            SourcePolicy::Node => "node",
            SourcePolicy::Own => "own",
            SourcePolicy::Choice => "choice",
        }
    }
    pub fn parse(s: &str) -> Option<SourcePolicy> {
        match s {
            "node" => Some(SourcePolicy::Node),
            "own" => Some(SourcePolicy::Own),
            "choice" => Some(SourcePolicy::Choice),
            _ => None,
        }
    }
}

/// The node's source policy. A node that never set one keeps playing as it did: [`SourcePolicy::Own`].
pub fn source_policy(lib: &pm_library::Library) -> SourcePolicy {
    lib.get_setting(SOURCE_POLICY_KEY)
        .ok()
        .flatten()
        .and_then(|v| SourcePolicy::parse(&v))
        .unwrap_or(SourcePolicy::Own)
}

/// A listener's own choice, as saved (the default `node` when they never chose).
pub fn listener_use(lib: &pm_library::Library, key: &str) -> &'static str {
    match Who::Listener(key.to_string()).get(lib, SOURCE_USE_KEY).as_deref() {
        Some("own") => "own",
        _ => "node",
    }
}

/// Whether a listener plays through the node's sign-ins now.
pub fn plays_on_node(lib: &pm_library::Library, key: &str) -> bool {
    match source_policy(lib) {
        SourcePolicy::Node => true,
        SourcePolicy::Own => false,
        SourcePolicy::Choice => listener_use(lib, key) == "node",
    }
}

/// The settings that hold a streaming sign-in for TIDAL, Qobuz and Spotify (and the sign-in way
/// picked): what [`Who::ListenerOnNode`] reads and refreshes at the node's keys.
pub const SIGNIN_KEYS: &[&str] = &[
    "tidal.user",
    "tidal.unofficial",
    "qobuz.unofficial",
    "spotify.user",
    "tidal.signin_way",
    "qobuz.signin_way",
    "spotify.signin_way",
];

tokio::task_local! {
    /// Who the request being answered is from (set around every handler by the middleware).
    pub static ASKING: Who;
}

/// Who is asking, for catalogue reads that use a streaming sign-in (search, item pages): the
/// request's person, resolved for the node's source policy ([`Who::resolved`]). Outside a request it fails closed: nobody's account (the services' own app
/// tokens at most), so a task that lost its request never borrows the owner's; work done for the
/// owner in the background says so ([`as_owner`]). Plays and sign-ins never use this: they take
/// their person explicitly.
pub fn asking() -> Who {
    ASKING.try_with(Who::clone).unwrap_or(Who::Nobody)
}

/// Runs `f` for the person asking now, in a task of its own: `tokio::spawn` alone would leave
/// the request's person behind.
pub fn spawn_asking<F>(f: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(ASKING.scope(asking(), f))
}

/// A sign-in at a streaming service that a listener started finishes only in the browser that
/// started it (or by that listener pasting the address): otherwise a listener could start one and
/// have the owner (or another listener) approve it, binding that person's account to their own.
/// The owner's need no binding: theirs can only become the owner's own sign-in.
pub fn login_binding(service: &str, who: &Who) -> Option<(String, String)> {
    if who.is_owner() {
        return None;
    }
    let value = crate::tidal::random_urlsafe(24);
    let header = format!("pm_login_{service}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age=900");
    Some((value, header))
}

/// How a streaming sign-in arrives back at the node.
pub enum LoginFinish {
    /// The service sent the browser to the callback: its cookies.
    Browser(axum::http::HeaderMap),
    /// Someone pasted the address the service sent them to.
    Pasted(Who),
}

/// Whether a sign-in may finish this way (see [`login_binding`]).
pub fn check_login(
    service: &str,
    started_by: &Who,
    binding: Option<&str>,
    finish: &LoginFinish,
) -> Result<(), ApiError> {
    if started_by.is_owner() {
        return Ok(());
    }
    let ok = match finish {
        LoginFinish::Pasted(w) => w == started_by,
        LoginFinish::Browser(h) => {
            let want = format!("pm_login_{service}=");
            let cookie = h
                .get_all(axum::http::header::COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(';'))
                .find_map(|c| c.trim().strip_prefix(&want).map(str::to_string));
            binding.is_some() && cookie.as_deref() == binding
        }
    };
    ok.then_some(()).ok_or_else(|| {
        ApiError::bad_request("This sign-in was started in another browser: start it again here")
    })
}

/// Runs `f` as the owner: the node's own background work that uses the owner's sign-ins.
pub async fn as_owner<F: std::future::Future>(f: F) -> F::Output {
    ASKING.scope(Who::Owner, f).await
}

/// Deletes every person's copy of a setting (the owner's and each listener's): a node-wide app
/// changed, so the sign-ins made with the old one go.
pub fn forget_everyones(lib: &pm_library::Library, base: &str) -> Result<(), ApiError> {
    lib.delete_setting(base)?;
    for (k, _) in lib.settings_with_prefix(PERSON_PREFIX)? {
        if k.ends_with(&format!(".{base}")) {
            lib.delete_setting(&k)?;
        }
    }
    Ok(())
}

impl Who {
    /// The person making a request.
    pub fn of(c: &Caller) -> Who {
        match (c.role, &c.key) {
            (Role::Owner, _) => Who::Owner,
            (Role::Listener, Some(k)) => Who::Listener(k.clone()),
            _ => Who::Nobody,
        }
    }

    /// The person a queue item plays for: who queued it; the owner's own items carry no one and
    /// no fetch limit, anyone else's an item someone queued without an account does.
    pub fn of_item(it: &pm_zones::QueueItem) -> Who {
        match &it.by {
            Some(k) => Who::Listener(k.clone()),
            None if it.limit.is_some() => Who::Nobody,
            None => Who::Owner,
        }
    }

    /// [`Who::of_item`], checked against People: a listener whose music access was taken away
    /// plays nothing with their sign-ins any more, items queued before included.
    pub fn of_item_in(it: &pm_zones::QueueItem, lib: &pm_library::Library) -> Who {
        match Who::of_item(it) {
            Who::Listener(k) if !crate::people::listed_for_music(lib, &k) => Who::Nobody,
            w => w.resolved(lib),
        }
    }

    /// The person whose account a play, a stream or a catalogue read uses, under the node's source
    /// policy: a listener who plays through the node's sign-ins becomes [`Who::ListenerOnNode`];
    /// everyone else is unchanged. Signing in, out and a person's own status never resolve: they
    /// act on the caller's own sign-ins.
    pub fn resolved(self, lib: &pm_library::Library) -> Who {
        match self {
            Who::Listener(k) if plays_on_node(lib, &k) => Who::ListenerOnNode(k),
            w => w,
        }
    }

    /// The person themselves, for their own sign-ins and settings: undoes [`Who::resolved`].
    pub fn own(&self) -> Who {
        match self {
            Who::ListenerOnNode(k) => Who::Listener(k.clone()),
            w => w.clone(),
        }
    }

    /// What a queue item records about who queued it.
    pub fn by(&self) -> Option<String> {
        match self {
            Who::Listener(k) | Who::ListenerOnNode(k) => Some(k.clone()),
            _ => None,
        }
    }

    pub fn is_owner(&self) -> bool {
        *self == Who::Owner
    }

    /// A name for this person in cache keys: `owner`, a listener's key, `node:<key>` for a listener
    /// on the node's sign-ins, or `nobody`. The caches keyed by it hold what an account answered
    /// (search, item pages, Home, streams), so a listener on the node's sign-ins and the same
    /// listener on their own never share an entry: a switch never shows one account's answers as
    /// the other's.
    pub fn tag(&self) -> String {
        match self {
            Who::Owner => "owner".into(),
            Who::Listener(k) => k.clone(),
            Who::ListenerOnNode(k) => format!("node:{k}"),
            Who::Nobody => "nobody".into(),
        }
    }

    /// This person's copy of a setting: the owner's is `base` itself, a listener's
    /// `person.<key>.<base>`; a listener on the node's sign-ins reads and refreshes those at the
    /// node's keys and everything else at their own; none for [`Who::Nobody`].
    pub fn setting(&self, base: &str) -> Option<String> {
        match self {
            Who::Owner => Some(base.to_string()),
            Who::ListenerOnNode(_) if SIGNIN_KEYS.contains(&base) => Some(base.to_string()),
            Who::Listener(k) | Who::ListenerOnNode(k) => Some(format!("{PERSON_PREFIX}{k}.{base}")),
            Who::Nobody => None,
        }
    }

    /// Reads this person's copy of a setting.
    pub fn get(&self, lib: &pm_library::Library, base: &str) -> Option<String> {
        lib.get_setting(&self.setting(base)?).ok().flatten()
    }

    /// Writes (or with `None` removes) this person's copy of a setting; refused for
    /// [`Who::Nobody`], and a write for a listener no longer in People with music access: a
    /// sign-in still on the way when they were removed (the service answering later, a token
    /// refresh) never lands again. Checked under the caller's library guard, the one People and
    /// [`forget_person`] write under.
    pub fn put(&self, lib: &pm_library::Library, base: &str, value: Option<&str>) -> Result<(), ApiError> {
        let Some(key) = self.setting(base) else {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "sign in to the node first: sign-ins are kept per person",
            ));
        };
        if let (Who::Listener(k) | Who::ListenerOnNode(k), Some(_)) = (self, value) {
            if !crate::people::listed_for_music(lib, k) {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "you no longer have music access on this node",
                ));
            }
        }
        match value {
            Some(v) => lib.set_setting(&key, v)?,
            None => {
                lib.delete_setting(&key)?;
            }
        }
        Ok(())
    }
}

/// A listener's token, as the node keeps it: the hash only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonToken {
    pub hash: String,
    pub key: String,
    pub created_at: i64,
}

fn hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn load(s: &NodeState, setting: &str) -> Vec<PersonToken> {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(setting)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

/// Changes a token list in place: read and saved under one hold of the library, so two changes
/// at once (a log in while Make a new token ends every session) can't bring ended ones back.
fn update<R>(
    s: &NodeState,
    setting: &str,
    f: impl FnOnce(&mut Vec<PersonToken>) -> R,
) -> Result<R, ApiError> {
    update_in(&s.library.lock().expect("library poisoned"), setting, f)
}

/// [`update`] with the library already held.
fn update_in<R>(
    lib: &pm_library::Library,
    setting: &str,
    f: impl FnOnce(&mut Vec<PersonToken>) -> R,
) -> Result<R, ApiError> {
    let mut all: Vec<PersonToken> = lib
        .get_setting(setting)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let r = f(&mut all);
    lib.set_setting(setting, &serde_json::to_string(&all).expect("serializes"))?;
    Ok(r)
}

/// What [`OWNER_SESSIONS_TOKEN`] keeps: a hash of the API token in use (none: of nothing).
fn token_mark(s: &NodeState) -> String {
    hash(&s.token().unwrap_or_default())
}

fn load_tokens(s: &NodeState) -> Vec<PersonToken> {
    load(s, TOKENS)
}

/// A new personal token for `key` (returned once; the node keeps its hash).
pub fn mint(s: &NodeState, key: &str) -> Result<String, ApiError> {
    let key = key.trim_start_matches("pubky").to_string();
    let token = format!("pml_{}", crate::tidal::random_urlsafe(32));
    let mut evicted = false;
    let done = update(s, TOKENS, |all| {
        // A person signing in again on another device keeps their other sessions; at most 10
        let mine: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, t)| t.key == key)
            .map(|(i, _)| i)
            .collect();
        if mine.len() >= 10 {
            all.remove(mine[0]);
            evicted = true;
        }
        all.push(PersonToken {
            hash: hash(&token),
            key: key.clone(),
            created_at: now(),
        });
    });
    // Their oldest session ended: a request let in with it changes no usage insights (usage.rs),
    // also when the write failed after
    if evicted {
        s.usage.revoke_listener(&key);
    }
    done?;
    Ok(token)
}

/// A new browser session for the owner (key `key`), from a Pubky Ring sign-in: returned once,
/// the node keeps its hash. At most `OWNER_SESSIONS_CAP`; the oldest goes first.
pub fn mint_owner(s: &NodeState, key: &str) -> Result<String, ApiError> {
    mint_owners(s, key, OWNER_SESSIONS, OWNER_SESSIONS_CAP)
}

/// A sign-in for one of the owner's devices (the app), from a Pubky Ring sign-in the owner
/// approved: as [`mint_owner`], in the devices' own list (`DEVICE_SESSIONS`).
pub fn mint_device(s: &NodeState, key: &str) -> Result<String, ApiError> {
    mint_owners(s, key, DEVICE_SESSIONS, DEVICE_SESSIONS_CAP)
}

/// `token`, `key`'s, into `all`, within `cap` (the oldest goes first); whether one went.
fn remember(all: &mut Vec<PersonToken>, key: &str, token: &str, cap: usize) -> bool {
    let mut evicted = false;
    while all.len() >= cap {
        all.remove(0);
        evicted = true;
    }
    all.push(PersonToken {
        hash: hash(token),
        key: key.to_string(),
        created_at: now(),
    });
    evicted
}

fn mint_owners(s: &NodeState, key: &str, list: &str, cap: usize) -> Result<String, ApiError> {
    let token = format!("{OWNER_PREFIX}{}", crate::tidal::random_urlsafe(32));
    let key = key.trim_start_matches("pubky").to_string();
    let lib = s.library.lock().expect("library poisoned");
    let mut evicted = false;
    let made = update_in(&lib, list, |all| {
        // Still the node's identity, checked under the same hold as the cleanup when it takes on
        // another one (`keep_owner_sessions_of`), so no session of the old one slips past it
        let current = s
            .sync_handle()
            .is_some_and(|h| h.z32().trim_start_matches("pubky") == key);
        if current {
            evicted = remember(all, &key, &token, cap);
        }
        current
    });
    // The oldest browser session or device sign-in ended: its usage insights end too, also when
    // a write failed after
    if evicted {
        s.usage.revoke_owner();
    }
    let made = made?;
    if made {
        lib.set_setting(OWNER_SESSIONS_TOKEN, &token_mark(s))?;
    }
    drop(lib);
    if !made {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "the node is signed in as someone else now: log in again",
        ));
    }
    Ok(token)
}

/// The answer to an owner's browser session while the node has no Pubky identity loaded (503, not
/// the 401 that makes the page log in again: the session works again once the identity is).
pub const IDENTITY_NOT_LOADED: &str = "this node couldn't load its Pubky identity (its homeserver didn't answer when it started), and browser sessions wait for it: restart the node once the homeserver answers. Until then the node's API token logs in";

/// An owner's browser session or device sign-in that is still open, for `owner`: the identity the
/// node has loaded now (a node signed in as someone else no longer lets the old owner's in).
fn owner_session(s: &NodeState, token: &str, owner: &str) -> bool {
    let h = hash(token);
    [OWNER_SESSIONS, DEVICE_SESSIONS]
        .iter()
        .any(|list| load(s, list).iter().any(|t| t.hash == h && t.key == owner))
}

/// The node took on the identity `owner` (restored at start, or a log in renewed or changed it):
/// browser sessions and device sign-ins of any other identity end, so none can come back later.
pub fn keep_owner_sessions_of(s: &NodeState, owner: &str) -> Result<(), ApiError> {
    let owner = owner.trim_start_matches("pubky");
    let mut ended = false;
    let browsers = update(s, OWNER_SESSIONS, |all| {
        let before = all.len();
        all.retain(|t| t.key == owner);
        ended |= all.len() < before;
    });
    let devices = update(s, DEVICE_SESSIONS, |all| {
        let before = all.len();
        all.retain(|t| t.key == owner);
        ended |= all.len() < before;
    });
    // A browser session or a device sign-in ended: its usage insights end too, also when a write
    // failed after
    if ended {
        s.usage.revoke_owner();
    }
    browsers?;
    devices
}

/// Ends one owner's browser session (Log out); whether it was open. Also while the node has no
/// identity loaded, when the session is otherwise refused: Log out must still work then. A
/// device's sign-in is not a browser session: the page in the app forgets it, the app keeps it
/// (as with the API token), and `false` says so.
pub fn end_owner_session(s: &NodeState, token: &str) -> Result<bool, ApiError> {
    let h = hash(token);
    let mut ended = false;
    let done = update(s, OWNER_SESSIONS, |all| {
        let before = all.len();
        all.retain(|t| t.hash != h);
        ended = all.len() < before;
        ended
    });
    // Its usage insights end too, and a request let in with it changes nothing (usage.rs)
    if ended {
        s.usage.revoke_owner();
    }
    done
}

/// At start: the API token differs from the one the owner's browser sessions were made under (it
/// was changed in the config, `PM_TOKEN` or `[api] token`), so they end, as with Make a new token.
/// Whether any did.
pub fn end_sessions_of_an_old_token(s: &NodeState) -> Result<bool, ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    let mark = token_mark(s);
    let was = lib.get_setting(OWNER_SESSIONS_TOKEN).ok().flatten();
    let ended = match was.as_deref() {
        Some(w) if w != mark => {
            let browsers = update_in(&lib, OWNER_SESSIONS, |all| {
                let any = !all.is_empty();
                all.clear();
                any
            })?;
            let devices = update_in(&lib, DEVICE_SESSIONS, |all| {
                let any = !all.is_empty();
                all.clear();
                any
            })?;
            browsers || devices
        }
        _ => false,
    };
    if was.as_deref() != Some(mark.as_str()) {
        lib.set_setting(OWNER_SESSIONS_TOKEN, &mark)?;
    }
    drop(lib);
    if ended {
        s.usage.revoke_owner();
    }
    Ok(ended)
}

/// Ends every browser session and device sign-in of the owner (a new API token: the answer to a
/// leak). The app signs in again by itself, with one approval in Pubky Ring.
pub fn end_owner_sessions(s: &NodeState) -> Result<(), ApiError> {
    let browsers = update(s, OWNER_SESSIONS, Vec::clear);
    let devices = update(s, DEVICE_SESSIONS, Vec::clear);
    s.usage.revoke_owner();
    browsers?;
    devices
}

/// Whose token this is, when it is a listener's that still has music access.
fn listener_of(s: &NodeState, token: &str) -> Option<String> {
    if !token.starts_with("pml_") {
        return None;
    }
    let h = hash(token);
    let key = load_tokens(s).into_iter().find(|t| t.hash == h)?.key;
    crate::people::may_use_music(s, &key).then_some(key)
}

/// The zones a listener may play on: the owner's list, or every zone when none is set.
pub fn listener_zones(s: &NodeState) -> Option<Vec<String>> {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(ZONES)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
        .filter(|v| !v.is_empty())
}

/// Routes a listener may never use (reads included): the owner's settings and data.
const OWNER_ONLY: &[&str] = &[
    // The owner's listen history (under /me, which a listener may otherwise use)
    "/me/listens",
    "/people",
    "/vault",
    "/party",
    "/remote",
    "/homeserver",
    "/node",
    "/pubky",
    "/backup",
    "/storage",
    "/library/webdav",
    "/library/uploads",
    "/library/imports",
    "/library/folders",
    "/library/tree",
    "/library/scan",
    "/library/tag-fixes",
    "/outputs",
];

/// What a listener may read: the shared library, public catalogues and directories, their
/// zones, and who they are. Everything else (the owner's bookmarks, playlists, tags, reviews,
/// history, podcast subscriptions and streaming accounts) stays the owner's.
const LISTENER_READS: &[&str] = &[
    "/me",
    "/events",
    "/info",
    "/status",
    "/health",
    "/library/albums",
    "/library/artists",
    "/library/tracks",
    "/library/genres",
    "/library/search",
    "/library/stats",
    "/lyrics",
    "/search",
    "/item",
    "/catalog/search",
    "/discover",
    "/shared",
    "/profiles",
    "/mb",
    "/radio/countries",
    "/radio/tags",
    "/radio/search",
    "/radio/curated",
    "/podcasts/search",
    "/zones",
    "/devices",
    "/presence",
];

/// A listener's own streaming sign-ins and plays (TASK-per-person-service-accounts): each of these
/// acts on the caller's own account, reads included; the apps and clients behind them stay the
/// owner's (the handlers refuse those fields, and [`LISTENER_OWN_EXCEPTIONS`] those routes).
const LISTENER_OWN: &[&str] = &[
    // Usage insights: their own choice, aliases and events (usage.rs checks the person itself)
    "/usage",
    "/play-order",
    "/sources/tidal/signin",
    "/sources/qobuz/signin",
    "/sources/spotify/signin",
    "/sources/tidal/login",
    "/sources/tidal/status",
    "/sources/tidal/session",
    "/sources/tidal/callback",
    "/sources/tidal/unofficial",
    "/sources/tidal/stream",
    "/sources/qobuz/unofficial",
    "/sources/qobuz/stream",
    "/sources/spotify/login",
    "/sources/spotify/status",
    "/sources/spotify/session",
    "/sources/spotify/callback",
    "/sources/spotify/devices",
    "/sources/spotify/play",
];

/// Inside [`LISTENER_OWN`], what goes on under every source policy: plays, streams and the play order.
const LISTENER_OWN_ALWAYS: &[&str] = &[
    // Usage insights aren't a sign-in: a listener's own choice, Stop for me and Clear mine work
    // whatever plays through whose sign-ins (usage.rs checks the person)
    "/usage",
    "/play-order",
    "/sources/tidal/stream",
    "/sources/qobuz/stream",
    "/sources/spotify/devices",
    "/sources/spotify/play",
];

/// Inside [`LISTENER_OWN`], a listener's own sign-in state, which they may still read (GET) when the
/// node plays through its own sign-ins.
const LISTENER_OWN_STATUS: &[&str] = &[
    "/sources/tidal/signin",
    "/sources/qobuz/signin",
    "/sources/spotify/signin",
    "/sources/tidal/status",
    "/sources/spotify/status",
    "/sources/tidal/session",
    "/sources/spotify/session",
    "/sources/tidal/unofficial",
    "/sources/qobuz/unofficial",
];

/// Why a listener can't sign in to their own service under [`SourcePolicy::Node`].
pub const NODE_SOURCES_ONLY: &str =
    "this node plays everyone's music through its own sign-ins (its owner chose so in Settings)";

/// Inside [`LISTENER_OWN`], still the owner's: checking the node's TIDAL device client and Qobuz app.
const LISTENER_OWN_EXCEPTIONS: &[&str] = &[
    "/sources/tidal/unofficial/client",
    "/sources/qobuz/unofficial/app",
];

/// Inside [`LISTENER_READS`], still the owner's: identification candidates and suggestions.
const LISTENER_READ_EXCEPTIONS: &[&str] = &["/candidates", "/suggestions"];

fn under(path: &str, prefixes: &[&str]) -> bool {
    prefixes
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

/// Whether a listener may use this zone: the owner's list, or every zone when none is set.
fn zone_refusal(s: &NodeState, zone: &str) -> Option<&'static str> {
    match listener_zones(s) {
        // A listen-along group is open when every zone it plays for is (presence.rs)
        Some(allowed)
            if !allowed.iter().any(|z| z == zone)
                && !s
                    .presence
                    .sources(zone)
                    .is_some_and(|zs| zs.iter().all(|z| allowed.contains(z))) =>
        {
            Some("the owner has not opened that zone to listeners")
        }
        _ => None,
    }
}

/// Whether this caller may see and play on `zone`: the owner every zone; a listener the zones
/// the owner opened to listeners (every zone when none are chosen); a party guest none.
pub fn may_use_zone(s: &NodeState, c: &Caller, zone: &str) -> bool {
    match c.role {
        Role::Owner => true,
        Role::Listener => zone_refusal(s, zone).is_none(),
        Role::Guest => false,
    }
}

/// Whether a listener may make this request. `None` = yes; `Some(why)` = no.
pub fn listener_refusal(s: &NodeState, method: &Method, path: &str) -> Option<&'static str> {
    // Inside the `/v1` router the prefix is gone; take either
    let path = path.strip_prefix("/v1").unwrap_or(path);
    if under(path, OWNER_ONLY) {
        return Some("that is the owner's");
    }
    // Their own streaming accounts
    if under(path, LISTENER_OWN) {
        if under(path, LISTENER_OWN_EXCEPTIONS) {
            return Some("that is the owner's");
        }
        // The node plays through its own sign-ins: a listener can't sign in to their own (what
        // they had stays; reads of their own status, plays and streams go on)
        if !under(path, LISTENER_OWN_ALWAYS)
            && !(*method == Method::GET && LISTENER_OWN_STATUS.contains(&path))
            // Signing out of their own stays possible
            && !(*method == Method::DELETE && LISTENER_OWN_STATUS.contains(&path))
            && source_policy(&s.library.lock().expect("library poisoned")) == SourcePolicy::Node
        {
            return Some(NODE_SOURCES_ONLY);
        }
        return None;
    }
    if *method == Method::GET || *method == Method::HEAD {
        if !under(path, LISTENER_READS) || LISTENER_READ_EXCEPTIONS.iter().any(|x| path.contains(x)) {
            return Some("that is the owner's");
        }
        // A zone's state and queue: only the zones open to listeners (the list itself is fine)
        if let Some(rest) = path.strip_prefix("/zones/") {
            return zone_refusal(s, rest.split('/').next().unwrap_or(rest));
        }
        return None;
    }
    // Playing on a zone the owner allows: its queue, transport, volume and the like
    if let Some(rest) = path.strip_prefix("/zones/") {
        let (zone, what) = rest.split_once('/').unwrap_or((rest, ""));
        if what.is_empty() || what.starts_with("dsp") || what == "crossfade" {
            return Some("only the owner adds, removes or tunes zones");
        }
        return zone_refusal(s, zone);
    }
    // The page asks who is signed in, and a listener may sign out
    if path == "/me" || path.starts_with("/me/") {
        return None;
    }
    // Their own phones and laptops and what those keep offline (devices.rs checks the owner)
    if path.starts_with("/devices/") {
        return None;
    }
    // Listening along (presence.rs checks the zones)
    if path == "/presence/along" {
        return None;
    }
    Some("listeners' own playlists, bookmarks and history are not built yet; this change would land in the owner's")
}

/// The request's caller, from its token: the API token or an owner's browser session is the
/// owner; a listener's personal token is a listener. `None` = not signed in.
pub fn caller(s: &NodeState, token: Option<&str>) -> Option<Caller> {
    let owner_key = || {
        s.sync_handle()
            .map(|h| h.z32().trim_start_matches("pubky").to_string())
    };
    let Some(api) = s.token() else {
        return Some(Caller {
            role: Role::Owner,
            key: owner_key(),
        });
    };
    let t = token?;
    if t == api.as_str() {
        return Some(Caller {
            role: Role::Owner,
            key: owner_key(),
        });
    }
    if t.starts_with(OWNER_PREFIX) {
        // Only for the identity the node has loaded. Without one (its homeserver didn't answer at
        // start) no session is the owner: which identity is the owner then can't be told, and an
        // earlier owner's session must not come back. The API token still logs in meanwhile
        let key = owner_key()?;
        return owner_session(s, t, &key).then_some(Caller {
            role: Role::Owner,
            key: Some(key),
        });
    }
    if t.starts_with(crate::party::GUEST_PREFIX) {
        return crate::party::guest_of(s, t).map(|_| Caller {
            role: Role::Guest,
            key: None,
        });
    }
    listener_of(s, t).map(|key| Caller {
        role: Role::Listener,
        key: Some(key),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Me {
    pub role: Role,
    pub key: Option<String>,
    /// For a listener: the zones they may play on (`null` = all).
    pub zones: Option<Vec<String>>,
}

/// `GET /v1/me`: who this token is.
pub(crate) async fn me(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<Caller>,
) -> ApiResult<Me> {
    Ok(Json(Me {
        zones: (c.role == Role::Listener).then(|| listener_zones(&s)).flatten(),
        role: c.role,
        key: c.key,
    }))
}

/// `DELETE /v1/me/token`: Log out of this browser. A listener's token or an owner's browser
/// session stops working; the node's API token (given to this browser by hand) stays as it is,
/// for the apps that use it: the page forgets it. `ended` says whether a session was ended.
pub(crate) async fn sign_out(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<Caller>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult<serde_json::Value> {
    // The token the request was let in with, from the header or `?token=` (as the middleware read it)
    let token = crate::bearer(&headers, &query).unwrap_or_default();
    match c.role {
        Role::Listener => {
            let h = hash(&token);
            let done = update(&s, TOKENS, |all| all.retain(|t| t.hash != h));
            if let Some(key) = &c.key {
                s.usage.revoke_listener(key);
            }
            done?;
        }
        Role::Owner if token.starts_with(OWNER_PREFIX) => {
            let ended = end_owner_session(&s, &token)?;
            return Ok(Json(serde_json::json!({"signed_out": true, "ended": ended})));
        }
        _ => return Ok(Json(serde_json::json!({"signed_out": true, "ended": false}))),
    }
    Ok(Json(serde_json::json!({"signed_out": true, "ended": true})))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListenerZones {
    /// Zone ids listeners may play on; empty = every zone.
    pub zones: Vec<String>,
}

/// `GET /v1/people/zones`: the zones open to listeners (empty = all).
pub(crate) async fn get_zones(State(s): State<Arc<NodeState>>) -> ApiResult<ListenerZones> {
    Ok(Json(ListenerZones {
        zones: listener_zones(&s).unwrap_or_default(),
    }))
}

/// `PUT /v1/people/zones {zones}`.
pub(crate) async fn set_zones(
    State(s): State<Arc<NodeState>>,
    Json(b): Json<ListenerZones>,
) -> ApiResult<ListenerZones> {
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(ZONES, &serde_json::to_string(&b.zones).expect("serializes"))?;
    Ok(Json(b))
}

/// Signed-in devices per person, for People.
pub fn sessions_of(s: &NodeState, key: &str) -> usize {
    load_tokens(s).iter().filter(|t| t.key == key).count()
}

/// Forgets every token of `key` (People › remove).
pub fn sign_out_everywhere(s: &NodeState, key: &str) -> Result<(), ApiError> {
    let done = update(s, TOKENS, |all| all.retain(|t| t.key != key));
    s.usage.revoke_listener(key);
    done
}

/// Everything the node keeps for one listener goes (People › Remove): their own streaming
/// sign-ins and settings (`person.<key>.*`), their tokens, preferences, presence setting and
/// devices, and what was fetched with their sign-ins (TIDAL tracks, search answers, item pages).
pub fn forget_person(s: &NodeState, key: &str) -> Result<(), ApiError> {
    let signed_out = sign_out_everywhere(s, key);
    // Their usage choice and what is still linked to them go (usage.rs), also when the token list
    // could not be written: they are already off the roster, so a retried remove never comes
    s.usage.forget_listener(key);
    signed_out?;
    // Their sign-ins still on the way: they would land again when the service answers
    let theirs = Who::Listener(key.to_string());
    crate::tidal::pending()
        .lock()
        .expect("pending")
        .retain(|_, p| p.who != theirs);
    crate::spotify::pending()
        .lock()
        .expect("pending")
        .retain(|_, p| p.who != theirs);
    {
        let lib = s.library.lock().expect("library poisoned");
        for (k, _) in lib.settings_with_prefix(&format!("{PERSON_PREFIX}{key}."))? {
            lib.delete_setting(&k)?;
        }
        lib.delete_setting(&crate::prefs::storage_key_of(key))?;
        lib.delete_setting(&crate::presence::visible_key(key))?;
        // What they read with their own sign-ins, and with the node's ([`Who::tag`])
        let on_node = Who::ListenerOnNode(key.to_string()).tag();
        for tag in [key, on_node.as_str()] {
            for (k, _) in lib.settings_with_prefix(&crate::discover::contents_prefix_of(tag))? {
                lib.delete_setting(&k)?;
            }
        }
    }
    let on_node = Who::ListenerOnNode(key.to_string()).tag();
    crate::tidal::forget_streams_of(key);
    crate::tidal::forget_streams_of(&on_node);
    crate::search::forget_answers_of(s, key);
    crate::search::forget_answers_of(s, &on_node);
    crate::devices::forget_all_of(s, key);
    Ok(())
}

/// The refusal a listener gets.
pub fn refused(why: &str) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, format!("Not for listeners: {why}."))
}

/// What a listener may put in a queue (or announce): library songs, the services' own track ids,
/// and http(s) links to public addresses, fetched only there (each redirect, playlist entry and
/// address it resolves to is checked when the node connects: the item carries
/// [`FetchLimit::PUBLIC`], whatever the request said). Never a path or a `file:` URL, nothing on
/// the node's machine or its network: those are the owner's to play. The URI is trimmed: the one
/// checked is the one queued.
pub fn check_listener_item(it: &mut pm_zones::QueueItem) -> Result<(), ApiError> {
    it.uri = it.uri.trim().to_string();
    it.limit = Some(it.limit.map_or(FetchLimit::PUBLIC, |l| l.and(FetchLimit::PUBLIC)));
    let uri = it.uri.clone();
    let uri = uri.as_str();
    use crate::*;
    const SERVICES: [&str; 13] = [
        sources::TRACK_SCHEME,
        tidal::TRACK_URI_PREFIX,
        deezer::TRACK_URI_PREFIX,
        qobuz::TRACK_URI_PREFIX,
        bandcamp::TRACK_URI_PREFIX,
        soundcloud::TRACK_URI_PREFIX,
        jamendo::TRACK_URI_PREFIX,
        opensubsonic::TRACK_URI_PREFIX,
        jellyfin::TRACK_URI_PREFIX,
        plex::TRACK_URI_PREFIX,
        audiobookshelf::TRACK_URI_PREFIX,
        youtube::VIDEO_URI_PREFIX,
        librespot::LIVE_URI,
    ];
    if SERVICES.iter().any(|p| uri.starts_with(p)) {
        return Ok(());
    }
    let public = FetchLimit::PUBLIC;
    let url = url::Url::parse(uri)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some());
    let Some(url) = url else {
        return Err(refused(
            "a listener queues songs from the library, the services' tracks and web links, not files or other addresses",
        ));
    };
    let literal = match url.host() {
        Some(url::Host::Ipv4(ip)) => Some(std::net::IpAddr::from(ip)),
        Some(url::Host::Ipv6(ip)) => Some(std::net::IpAddr::from(ip)),
        _ => None,
    };
    if literal.is_some_and(|ip| !sources::allows(public, ip)) || url.host_str() == Some("localhost") {
        return Err(refused("that address is on the node's own network"));
    }
    // The URL as parsed is what is queued: `HTTP://…` or `http:\\…` would not be taken for a web
    // link when it plays (review of #177)
    it.uri = url.to_string();
    Ok(())
}

/// A listener's JSON answers show a library file by its name, not by where the owner keeps it
/// (`file.path` of every track, in lists, albums, search and item pages alike).
pub async fn listener_response(
    resp: axum::response::Response,
    roots: &[String],
    origins: &[String],
    known: &(dyn Fn(&str) -> bool + Sync),
) -> axum::response::Response {
    use axum::http::header;
    use axum::response::IntoResponse;
    // Only application/json answers are rewritten: streams (search's NDJSON, events, audio, covers)
    // pass as they are, so a stream that ever carries file paths needs its own filtering.
    let json = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if !json {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "reading the answer failed").into_response();
    };
    let rewritten = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|mut v| {
            hide_for_listener(&mut v, roots, origins, known)
                .then(|| serde_json::to_vec(&v).ok())
                .flatten()
        });
    let body = match rewritten {
        Some(b) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            axum::body::Body::from(b)
        }
        None => bytes.into(),
    };
    axum::response::Response::from_parts(parts, body)
}

/// What a listener (or party guest) sees of an answer, JSON answers and event messages alike:
/// file names, not where the owner keeps files ([`hide_file_paths`], [`hide_paths_under`]), and
/// the node's links without a sign-in ([`hide_node_tokens`]). Whether anything changed.
pub fn hide_for_listener(
    v: &mut serde_json::Value,
    roots: &[String],
    origins: &[String],
    known: &dyn Fn(&str) -> bool,
) -> bool {
    hide_file_paths(v) | hide_paths_under(v, roots) | hide_node_tokens(v, origins, known)
}

/// Every `/v1/` link in the answer loses the `token` query parameters it loses
/// (`pm_zones::node_link_without_token`: all of them on the node's own links, the node's own
/// sign-ins on a link under any other name): a queue item's cover or stream link made by a
/// browser never hands that browser's sign-in to whoever else reads the zone. Queues drop them
/// on the way in (`pm_zones::QueueItem::without_node_token`); this is the second look, on every
/// answer. Whether anything changed.
pub fn hide_node_tokens(v: &mut serde_json::Value, origins: &[String], known: &dyn Fn(&str) -> bool) -> bool {
    use serde_json::Value;
    match v {
        Value::String(t) => match pm_zones::node_link_without_token(t, origins, known) {
            Some(clean) => {
                *t = clean;
                true
            }
            None => false,
        },
        Value::Object(o) => o
            .values_mut()
            .fold(false, |c, x| hide_node_tokens(x, origins, known) | c),
        Value::Array(a) => a
            .iter_mut()
            .fold(false, |c, x| hide_node_tokens(x, origins, known) | c),
        _ => false,
    }
}

/// The sign-ins this node knows as its own, for telling a link that carries one under another
/// name (`pm_zones::node_link_tokens`): the API token, and the hashes of the owner's browser
/// and device sessions and of the listeners' tokens. Read from the library once, on first use.
pub struct KnownTokens<'a> {
    s: &'a NodeState,
    set: std::sync::OnceLock<KnownSet>,
}

#[derive(Default)]
struct KnownSet {
    api: Option<String>,
    hashes: std::collections::HashSet<String>,
}

impl KnownSet {
    fn load(lib: &pm_library::Library, api: Option<String>) -> Self {
        let mut hashes = std::collections::HashSet::new();
        for list in [OWNER_SESSIONS, DEVICE_SESSIONS, TOKENS] {
            if let Some(all) = lib
                .get_setting(list)
                .ok()
                .flatten()
                .and_then(|j| serde_json::from_str::<Vec<PersonToken>>(&j).ok())
            {
                hashes.extend(all.into_iter().map(|t| t.hash));
            }
        }
        Self { api, hashes }
    }

    fn has(&self, t: &str) -> bool {
        self.api.as_deref() == Some(t) || self.hashes.contains(&hash(t))
    }
}

impl<'a> KnownTokens<'a> {
    pub fn lazy(s: &'a NodeState) -> Self {
        Self {
            s,
            set: std::sync::OnceLock::new(),
        }
    }

    /// Whether `t` is one of this node's sign-ins.
    pub fn has(&self, t: &str) -> bool {
        self.set
            .get_or_init(|| {
                let lib = self.s.library.lock().expect("library poisoned");
                KnownSet::load(&lib, self.s.token())
            })
            .has(t)
    }
}

/// [`KnownTokens::has`] for a queue's own check (`pm_zones::TokenCheck`), which may run while
/// another thread holds the library: it waits a little for it, and a link that can't be checked
/// then keeps its token, which every listener answer still hides ([`hide_node_tokens`]).
pub fn is_node_token_if_free(s: &NodeState, t: &str) -> bool {
    for _ in 0..20 {
        if let Ok(lib) = s.library.try_lock() {
            return KnownSet::load(&lib, s.token()).has(t);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

/// Setting: when the API token itself was found in a saved queue's link (Unix seconds). Home and
/// Settings say so until the token is replaced, which clears it.
pub const API_TOKEN_EXPOSED: &str = "security.api_token_exposed_at";

/// When the API token was last found in a saved queue's link, until it is replaced.
pub fn api_token_exposed_at(lib: &pm_library::Library) -> Option<i64> {
    lib.get_setting(API_TOKEN_EXPOSED)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
}

/// What cleaning the saved queues at start found: how many items carried a sign-in in one of
/// their node links, and what ended because of it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanedQueues {
    pub items: usize,
    /// The owner's browser sessions and device sign-ins ended (one of theirs, or the API token,
    /// was in a saved queue).
    pub owner_sessions_ended: bool,
    /// The node's API token itself was in a saved queue: it can only be replaced by hand.
    pub api_token_found: bool,
    /// Listeners' tokens that were in a saved queue and ended.
    pub listener_tokens_ended: usize,
}

/// At start, before the zones restore their queues: every saved queue (`queue.*`) loses the
/// `token` query parameters of its node links, and a sign-in of this node that was in one ends,
/// since the queue was readable by everyone who could use the zone: the owner's browser sessions
/// and device sign-ins (they sign in again by themselves), a listener's token. The API token is
/// the owner's to replace (Settings): reported. A token this node doesn't know ends nothing.
pub fn clean_saved_queues(s: &NodeState) -> Result<CleanedQueues, ApiError> {
    let mut out = CleanedQueues::default();
    let origins = s.node_origins();
    let mut found: Vec<String> = Vec::new();
    let lib = s.library.lock().expect("library poisoned");
    // A link under another of the node's names loses the node's own sign-ins too
    let known_set = KnownSet::load(&lib, s.token());
    let known = |t: &str| known_set.has(t);
    for (key, json) in lib.settings_with_prefix("queue.")? {
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&json) else {
            continue;
        };
        let Some(items) = v.get_mut("items").and_then(|i| i.as_array_mut()) else {
            continue;
        };
        let mut n = 0;
        for it in items.iter_mut() {
            let mut carried = false;
            for f in ["uri", "image"] {
                if let Some(serde_json::Value::String(link)) = it.get_mut(f) {
                    for t in pm_zones::node_link_tokens(link, &origins, &known) {
                        found.push(t.to_string());
                        carried = true;
                    }
                    if let Some(clean) = pm_zones::node_link_without_token(link, &origins, &known) {
                        *link = clean;
                    }
                }
            }
            n += usize::from(carried);
        }
        if n > 0 {
            out.items += n;
            lib.set_setting(&key, &v.to_string())?;
        }
    }
    if found.is_empty() {
        return Ok(out);
    }
    let api = s.token();
    let owners: Vec<String> = [OWNER_SESSIONS, DEVICE_SESSIONS]
        .iter()
        .flat_map(|list| {
            lib.get_setting(list)
                .ok()
                .flatten()
                .and_then(|j| serde_json::from_str::<Vec<PersonToken>>(&j).ok())
                .unwrap_or_default()
        })
        .map(|t| t.hash)
        .collect();
    let mut listener_keys = Vec::new();
    for t in &found {
        let h = hash(t);
        if api.as_deref() == Some(t.as_str()) {
            out.api_token_found = true;
        } else if owners.contains(&h) {
            out.owner_sessions_ended = true;
        } else if t.starts_with("pml_") {
            let key = update_in(&lib, TOKENS, |all| {
                let key = all.iter().find(|x| x.hash == h).map(|x| x.key.clone());
                all.retain(|x| x.hash != h);
                key
            })?;
            if let Some(key) = key {
                out.listener_tokens_ended += 1;
                listener_keys.push(key);
            }
        }
    }
    // The API token can't be replaced here: Home and Settings say so until the owner does
    if out.api_token_found {
        lib.set_setting(API_TOKEN_EXPOSED, &pm_library::db::now_unix().to_string())?;
    }
    // The owner's sign-ins end once, as with Make a new token: one of them was readable, and
    // with the API token, every session made under it should be made again too
    if out.owner_sessions_ended || out.api_token_found {
        out.owner_sessions_ended = true;
        update_in(&lib, OWNER_SESSIONS, Vec::clear)?;
        update_in(&lib, DEVICE_SESSIONS, Vec::clear)?;
    }
    drop(lib);
    if out.owner_sessions_ended {
        s.usage.revoke_owner();
    }
    for key in listener_keys {
        s.usage.revoke_listener(&key);
    }
    Ok(out)
}

/// Replaces the folder part of every library file's path with nothing, leaving the file name:
/// in objects shaped like `pm_model::AudioFile` (`path`, `container`, `size_bytes`), in the
/// song page's "File" fact (`{label: "File", value}`, item.rs) and in queue items whose `uri` is
/// a file on the node (`/…`, `C:\\…`, `file:…`); then wherever else those paths appear in the
/// answer (a zone's error names the file it could not play). Whether anything changed.
pub fn hide_file_paths(v: &mut serde_json::Value) -> bool {
    let mut seen = Vec::new();
    let mut changed = hide_in(v, &mut seen);
    if !seen.is_empty() {
        seen.sort_by_key(|p: &String| std::cmp::Reverse(p.len()));
        seen.dedup();
        changed |= replace_paths(v, &seen);
    }
    changed
}

/// The folders the owner keeps music in (library folders and the homeserver's files), for
/// [`hide_paths_under`].
pub fn library_roots(s: &NodeState) -> Vec<String> {
    let mut roots: Vec<String> = s
        .library
        .lock()
        .expect("library poisoned")
        .list_folders()
        .unwrap_or_default()
        .into_iter()
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .collect();
    if let Some(hs) = s.homeserver_files_dir() {
        roots.push(hs.to_string_lossy().into_owned());
    }
    roots.retain(|r| r.len() > 1);
    roots
}

/// Every path under one of `roots` in the answer's text, by its file name: a library song's
/// open error names its file (`cannot open /srv/music/A/x.flac: …`) where no queue item carries
/// it (review of #177). A path runs to `": "`, a quote or the end of the text.
pub fn hide_paths_under(v: &mut serde_json::Value, roots: &[String]) -> bool {
    use serde_json::Value;
    match v {
        Value::String(t) => {
            let mut changed = false;
            for root in roots {
                let root = root.trim_end_matches(['/', '\\']);
                let mut from = 0;
                while let Some(found) = t[from..].find(root) {
                    let at = from + found;
                    // A path on its own: at the start or after a space, quote, bracket, `=` or
                    // `:`, and the folder itself followed by a separator (never inside a URL)
                    let before_ok = t[..at]
                        .chars()
                        .next_back()
                        .is_none_or(|c| c.is_whitespace() || "\"'(=:".contains(c));
                    let after_ok = t[at + root.len()..].starts_with(['/', '\\']);
                    if !(before_ok && after_ok) {
                        from = at + root.len();
                        continue;
                    }
                    let rest = &t[at..];
                    let end = [": ", "\"", "\n"]
                        .iter()
                        .filter_map(|e| rest.find(e))
                        .min()
                        .unwrap_or(rest.len());
                    let name = file_name(rest[..end].trim_end_matches(['/', '\\']));
                    t.replace_range(at..at + end, &name);
                    from = at + name.len();
                    changed = true;
                }
            }
            changed
        }
        Value::Object(o) => o.values_mut().fold(false, |c, x| hide_paths_under(x, roots) | c),
        Value::Array(a) => a.iter_mut().fold(false, |c, x| hide_paths_under(x, roots) | c),
        _ => false,
    }
}

fn file_name(p: &str) -> String {
    p.rsplit(['/', '\\']).next().unwrap_or_default().to_string()
}

/// A queue item's `uri` that is a file on the node, not a link or a service's id.
fn local_path(u: &str) -> bool {
    let b = u.as_bytes();
    u.starts_with('/')
        || u.starts_with("\\\\")
        || u.starts_with("file:")
        || (b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/'))
}

fn hide_in(v: &mut serde_json::Value, seen: &mut Vec<String>) -> bool {
    use serde_json::Value;
    match v {
        Value::Object(o) => {
            let mut changed = false;
            let field = if o.contains_key("container") && o.contains_key("size_bytes") {
                Some("path")
            } else if o.get("label").and_then(Value::as_str) == Some("File") {
                Some("value")
            } else if o.get("uri").and_then(Value::as_str).is_some_and(local_path) {
                Some("uri")
            } else {
                None
            };
            if let Some(Value::String(p)) = field.and_then(|f| o.get_mut(f)) {
                let name = file_name(p);
                if name != *p {
                    seen.push(std::mem::replace(p, name));
                    changed = true;
                }
            }
            for x in o.values_mut() {
                changed |= hide_in(x, seen);
            }
            changed
        }
        Value::Array(a) => a.iter_mut().fold(false, |c, x| hide_in(x, seen) | c),
        _ => false,
    }
}

/// Every hidden path, wherever else it is in the answer's text, by its file name.
fn replace_paths(v: &mut serde_json::Value, paths: &[String]) -> bool {
    use serde_json::Value;
    match v {
        Value::String(t) => {
            let mut changed = false;
            for p in paths {
                if t.contains(p.as_str()) {
                    *t = t.replace(p.as_str(), &file_name(p));
                    changed = true;
                }
            }
            changed
        }
        Value::Object(o) => o.values_mut().fold(false, |c, x| replace_paths(x, paths) | c),
        Value::Array(a) => a.iter_mut().fold(false, |c, x| replace_paths(x, paths) | c),
        _ => false,
    }
}

/// Whose sign-ins a person plays TIDAL, Qobuz and Spotify with (`GET/PUT /v1/me/sources`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MySources {
    /// The node's policy: `node`, `own` or `choice`.
    pub policy: SourcePolicy,
    /// The person's own choice (`node` or `own`), kept whatever the policy.
    #[serde(rename = "use")]
    pub choice: String,
    /// Whose sign-ins plays use now: `node` or `own`.
    pub plays_with: String,
    /// Whether the person may switch (a listener, under `choice`).
    pub may_choose: bool,
}

/// What a person may change: their own choice.
#[derive(Debug, Clone, Deserialize)]
pub struct MySourcesUpdate {
    #[serde(rename = "use")]
    pub choice: String,
}

fn my_sources_view(lib: &pm_library::Library, who: &Who) -> MySources {
    let policy = source_policy(lib);
    match who {
        Who::Listener(k) | Who::ListenerOnNode(k) => MySources {
            policy,
            choice: listener_use(lib, k).into(),
            plays_with: if plays_on_node(lib, k) { "node" } else { "own" }.into(),
            may_choose: policy == SourcePolicy::Choice,
        },
        // The owner's sign-ins are the node's
        Who::Owner => MySources {
            policy,
            choice: "node".into(),
            plays_with: "node".into(),
            may_choose: false,
        },
        // A party guest or a LAN controller plays with no one's
        Who::Nobody => MySources {
            policy,
            choice: "node".into(),
            plays_with: "none".into(),
            may_choose: false,
        },
    }
}

/// `GET /v1/me/sources`.
pub(crate) async fn my_sources(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<Caller>,
) -> ApiResult<MySources> {
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(my_sources_view(&lib, &Who::of(&c))))
}

/// `PUT /v1/me/sources {use: "node"|"own"}`: a listener's choice, under the `choice` policy. Their own
/// sign-ins and the node's both stay as they are; only which one plays changes.
pub(crate) async fn set_my_sources(
    State(s): State<Arc<NodeState>>,
    axum::Extension(c): axum::Extension<Caller>,
    Json(b): Json<MySourcesUpdate>,
) -> ApiResult<MySources> {
    let who = Who::of(&c);
    let Who::Listener(_) = &who else {
        return Err(ApiError::bad_request("the owner's sign-ins are the node's"));
    };
    if !matches!(b.choice.as_str(), "node" | "own") {
        return Err(ApiError::bad_request("use is `node` or `own`"));
    }
    let lib = s.library.lock().expect("library poisoned");
    if source_policy(&lib) != SourcePolicy::Choice {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this node's owner decides whose sign-ins everyone plays with",
        ));
    }
    who.put(&lib, SOURCE_USE_KEY, Some(&b.choice))?;
    Ok(Json(my_sources_view(&lib, &who)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_links_in_an_answer_lose_their_token() {
        let mut v = serde_json::json!({
            "queue": [
                {"uri": "track:1", "image": "/v1/library/albums/7/cover?token=pmo_abc"},
                {"uri": "http://127.0.0.1:7788/v1/library/tracks/1/file?token=s3cret", "image": null},
                {"uri": "https://radio.example/live?token=theirs", "image": "https://cdn.example/a.jpg"}
            ],
            "now_playing": {"item": {"image": "/v1/library/albums/7/cover?a=1&token=pml_x#f"}},
            "error": "no token here"
        });
        let me = ["http://127.0.0.1:7788".to_string()];
        let no = |_: &str| false;
        assert!(hide_node_tokens(&mut v, &me, &no));
        let text = v.to_string();
        for t in ["pmo_abc", "s3cret", "pml_x"] {
            assert!(!text.contains(t), "{t}: {text}");
        }
        assert_eq!(v["queue"][0]["image"], "/v1/library/albums/7/cover");
        assert_eq!(
            v["queue"][1]["uri"],
            "http://127.0.0.1:7788/v1/library/tracks/1/file"
        );
        assert_eq!(v["queue"][2]["uri"], "https://radio.example/live?token=theirs");
        assert_eq!(
            v["now_playing"]["item"]["image"],
            "/v1/library/albums/7/cover?a=1#f"
        );
        assert!(!hide_node_tokens(&mut v, &me, &no), "nothing left to hide");
        // Another host's `/v1/` link keeps its own token
        let mut theirs = serde_json::json!(["https://other.example/v1/x?token=theirs"]);
        assert!(!hide_node_tokens(&mut theirs, &me, &no));
    }

    /// A library song's open error names its file only in its text: under the library's folders
    /// it is cut to the file name, links are left alone (review of #177).
    #[test]
    fn paths_under_the_library_folders_become_file_names() {
        let roots = vec!["/srv/music".to_string(), "C:\\Music\\".to_string()];
        let mut v = serde_json::json!({
            "error": "cannot open /srv/music/My Album/01 x.flac: no decoder",
            "reason": "C:\\Music\\A\\y.flac",
            "links": ["https://musicbrainz.org/srv/music/z", "http://nas/srv/music/a.mp3", "/srv/musical/b.flac"],
        });
        assert!(hide_paths_under(&mut v, &roots));
        assert_eq!(v["error"], "cannot open 01 x.flac: no decoder");
        assert_eq!(v["reason"], "y.flac");
        assert_eq!(
            v["links"],
            serde_json::json!([
                "https://musicbrainz.org/srv/music/z",
                "http://nas/srv/music/a.mp3",
                "/srv/musical/b.flac"
            ])
        );
    }

    #[test]
    fn listeners_see_file_names_only() {
        let mut v = serde_json::json!({"tracks": [
            {"title": "A", "file": {"path": "/srv/music/Owner/A.flac", "container": "flac", "size_bytes": 1}},
            {"title": "B", "file": {"path": "webdav://nas/share/B.mp3", "container": "mp3", "size_bytes": 2}},
            {"title": "C", "file": {"path": "C:\\Music\\C.flac", "container": "flac", "size_bytes": 3}},
            {"title": "D", "file": {"path": "D.flac", "container": "flac", "size_bytes": 4}},
        ], "folder": {"path": "/srv/music"},
           "about": [{"label": "Format", "value": "FLAC 16/44.1"}, {"label": "File", "value": "/srv/music/Owner/A.flac"}]});
        assert!(hide_file_paths(&mut v));
        let names: Vec<_> = v["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["file"]["path"].clone())
            .collect();
        assert_eq!(names, ["A.flac", "B.mp3", "C.flac", "D.flac"]);
        // The song page's File fact too; other facts stay
        assert_eq!(v["about"][1]["value"], "A.flac");
        assert_eq!(v["about"][0]["value"], "FLAC 16/44.1");
        // Only library files: another object's `path` stays
        assert_eq!(v["folder"]["path"], "/srv/music");
        let mut plain = serde_json::json!({"file": {"path": "x.flac", "container": "flac", "size_bytes": 1}});
        assert!(!hide_file_paths(&mut plain));
        // A zone: queue items that are files, and its error naming one (security review
        // 2026-10-30, low)
        let mut zone = serde_json::json!({"id": "z", "error": "cannot open /srv/music/My Album/x.flac: broken",
            "queue": [{"id": 1, "uri": "/srv/music/My Album/x.flac"}, {"id": 2, "uri": "C:\\Music\\y.flac"},
                      {"id": 3, "uri": "track:4"}, {"id": 4, "uri": "https://radio.example.org/s.mp3"}]});
        assert!(hide_file_paths(&mut zone));
        assert_eq!(zone["error"], "cannot open x.flac: broken");
        let uris: Vec<_> = zone["queue"]
            .as_array()
            .unwrap()
            .iter()
            .map(|q| q["uri"].clone())
            .collect();
        assert_eq!(
            uris,
            ["x.flac", "y.flac", "track:4", "https://radio.example.org/s.mp3"]
        );
    }

    /// Sessions are kept within their cap, the oldest going first; devices have a cap of their own.
    #[test]
    fn sessions_stay_within_their_cap_oldest_first() {
        let mut all = Vec::new();
        for i in 0..OWNER_SESSIONS_CAP + 3 {
            remember(&mut all, "k", &format!("pmo_{i}"), OWNER_SESSIONS_CAP);
        }
        assert_eq!(all.len(), OWNER_SESSIONS_CAP);
        assert_eq!(all[0].hash, hash("pmo_3"), "the three oldest went");
    }
}
