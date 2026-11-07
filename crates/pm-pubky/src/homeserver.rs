//! Read and write the music posts on homeservers through the `pubky` SDK
//! (0.16.2). The node writes the user's own posts with their session (to
//! the private folder by default) and reads anyone's public posts without
//! one. Private posts are listed and read only with the owner's session.
//!
//! Sign-in is Pubky Ring's `pubkyauth` flow (M9) and lives outside this crate;
//! here a session is either handed in or restored from an exported grant
//! secret with [`NodeHomeserver::restore_session`].
//!
//! API notes against 0.16.2 (the skill notes describe 0.12): `signin` takes a
//! `ClientId`, `signup` returns `()`, `restore_session(&str)` accepts both
//! grant and legacy cookie secrets, `PublicStorage::list` needs a trailing
//! `/` and returns `PubkyResource { owner, path }` entries, `get_json` is
//! behind the `json` feature.

use crate::config::Network;
use crate::schemas::{Activity, Bookmark, MusicPost, Playlist, Review, Visibility};
pub use pubky::PubkySession;
use pubky::{Pubky, PublicKey};
use serde::Serialize;

/// The homeserver answers 404 for a directory nobody has written to yet.
pub(crate) fn is_not_found(e: &pubky::Error) -> bool {
    matches!(
        e,
        pubky::Error::Request(pubky::errors::RequestError::Server { status, .. })
            if *status == reqwest::StatusCode::NOT_FOUND
    )
}

/// The homeserver answers 507 when the person's storage is full (their quota is used up).
pub fn is_full(e: &HomeserverError) -> bool {
    matches!(
        e,
        HomeserverError::Pubky(pubky::Error::Request(pubky::errors::RequestError::Server { status, .. }))
            if *status == reqwest::StatusCode::INSUFFICIENT_STORAGE
    )
}

#[derive(Debug, thiserror::Error)]
pub enum HomeserverError {
    #[error(transparent)]
    Pubky(#[from] pubky::Error),
    #[error("invalid public key: {0}")]
    Key(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("object at {path} is invalid: {source}")]
    Schema {
        path: String,
        source: crate::schemas::SchemaError,
    },
    #[error("reading private objects needs the owner's session")]
    NeedsSession,
    #[error("the homeserver's answer is over {max} bytes")]
    TooLarge { max: usize },
    #[error("the homeserver took too long to answer")]
    Timeout,
    #[error("the listing of {dir} did not end within its limits")]
    Cut { dir: String },
}

impl HomeserverError {
    /// The server or the network may answer the same request later: a transport failure or a
    /// 5xx answer.
    pub fn is_transient(&self) -> bool {
        match self {
            HomeserverError::Pubky(pubky::Error::Request(pubky::errors::RequestError::Transport(_))) => true,
            _ => self.is_server_error(),
        }
    }

    /// The server answered 5xx.
    pub fn is_server_error(&self) -> bool {
        matches!(
            self,
            HomeserverError::Pubky(pubky::Error::Request(pubky::errors::RequestError::Server { status, .. }))
                if status.is_server_error()
        )
    }
}

/// Reads in flight at once during a sync.
pub const FETCH_AT_ONCE: usize = 8;

/// The most of one object read into memory: a post, a bookmark, a profile, a playlist (social v1
/// caps a post at 512 KiB). Someone else's homeserver can answer anything, and a sync reads
/// [`FETCH_AT_ONCE`] of them at a time.
pub const OBJECT_MAX: usize = 1024 * 1024;

/// The most of a private file read whole into memory by [`NodeHomeserver::download_private`];
/// music files go to disk as they arrive with [`NodeHomeserver::download_private_to`].
pub const FILE_MAX: usize = 1024 * 1024 * 1024;

/// How long reading one object (or one page of a listing) may take, request and body. The Pubky
/// client has no request timeout of its own, and a client-wide one would also cut the long music
/// uploads and downloads, so each object read has its own.
pub const OBJECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a file download may go without receiving a byte: a download takes as long as it
/// takes (it may be paced to a rate), but a homeserver that stops sending is given up on.
pub const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A response's body, refused unread when it says it is over `max` bytes and given up as soon
/// as more than `max` arrive (a body need not say its length), or when nothing arrives for
/// `stall` ([`STALL_TIMEOUT`]).
async fn read_capped(
    mut resp: reqwest::Response,
    max: usize,
    stall: std::time::Duration,
) -> Result<Vec<u8>, HomeserverError> {
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Err(HomeserverError::TooLarge { max });
    }
    let mut out = Vec::new();
    while let Some(chunk) = within(stall, async {
        resp.chunk()
            .await
            .map_err(|e| HomeserverError::Transport(e.to_string()))
    })
    .await?
    {
        if out.len() + chunk.len() > max {
            return Err(HomeserverError::TooLarge { max });
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// `read` within `limit`, else [`HomeserverError::Timeout`].
async fn within<T>(
    limit: std::time::Duration,
    read: impl std::future::Future<Output = Result<T, HomeserverError>>,
) -> Result<T, HomeserverError> {
    tokio::time::timeout(limit, read)
        .await
        .unwrap_or(Err(HomeserverError::Timeout))
}

/// A tag read from a homeserver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTag {
    pub path: String,
    pub id: String,
    pub uri: String,
    pub label: String,
    pub created_at: i64,
}

/// What [`NodeHomeserver::read_music_tags_in`] found.
#[derive(Debug, Clone, Default)]
pub struct RemoteTags {
    pub tags: Vec<RemoteTag>,
    /// Ids of social v1 tags that are not on music (another app's): skip them next time.
    pub not_music: Vec<String>,
}

/// Default page size for listings; the homeserver may cap it.
pub const LIST_LIMIT: u16 = 100;

/// How far a listing goes. A page is one LIST request of [`LIST_LIMIT`] entries; the bounds
/// hold whatever the homeserver answers, so a listing of someone else's homeserver ends in
/// bounded time and memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListBounds {
    /// At most this many entries are kept; the rest of the directory is left unlisted.
    pub max_entries: usize,
    /// At most this many pages are asked for.
    pub max_pages: usize,
    /// For the whole listing, every page together (each page also has [`OBJECT_TIMEOUT`]).
    pub deadline: std::time::Duration,
    /// Newest first: a cut listing of timestamp-id paths then keeps the newest entries.
    pub newest_first: bool,
    /// The listing must end within the bounds: a cut one is [`HomeserverError::Cut`] (a sync
    /// must never take a partial listing for the whole; it would read the rest as deleted).
    pub whole: bool,
}

impl ListBounds {
    /// Another person's homeserver, read for a shared view: their newest posts (every version
    /// of a post lies under its id, so 2,000 entries hold well over the 1,000 posts a view
    /// reads), 25 pages, 20 s. A cut listing is still a listing, of the newest.
    pub const SHARED: Self = Self {
        max_entries: 2_000,
        max_pages: 25,
        deadline: std::time::Duration::from_secs(20),
        newest_first: true,
        whole: false,
    };
    /// The owner's own homeserver: complete, but finite (200,000 entries, 2,000 pages, 10 min).
    /// A listing that reaches a limit is an error, never a partial listing taken as complete.
    pub const OWNER: Self = Self {
        max_entries: 200_000,
        max_pages: 2_000,
        deadline: std::time::Duration::from_secs(600),
        newest_first: false,
        whole: true,
    };
}

/// What a bounded listing found: `complete` when the directory ended within the bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing<T = String> {
    pub entries: Vec<T>,
    pub complete: bool,
}

/// One page of a listing: each entry with the cursor that continues after it (the entry's
/// pubky URL).
pub(crate) type Page<T> = Vec<(T, String)>;

/// A listing within `bounds`, page by page from `page(cursor)`, which answers `Ok(None)` for a
/// directory that isn't there (it lists as empty). The listing stops, incomplete, at the entry
/// cap (entries past it are dropped, from the page that crosses it too), the page cap, the
/// deadline, or a cursor that doesn't move on (one seen before); with `bounds.whole` that is
/// [`HomeserverError::Cut`] instead.
pub(crate) async fn list_within<T, F, Fut>(
    bounds: ListBounds,
    dir: &str,
    mut page: F,
) -> Result<Listing<T>, HomeserverError>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Option<Page<T>>, HomeserverError>>,
{
    let started = std::time::Instant::now();
    let mut entries = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::HashSet::new();
    let mut complete = false;
    for _ in 0..bounds.max_pages {
        let left = bounds.deadline.saturating_sub(started.elapsed());
        let Ok(got) = tokio::time::timeout(left, page(cursor.clone())).await else {
            break;
        };
        let Some(got) = got? else {
            complete = true;
            break;
        };
        let n = got.len();
        let next = got.last().map(|(_, c)| c.clone());
        entries.extend(got.into_iter().map(|(e, _)| e));
        if entries.len() > bounds.max_entries {
            entries.truncate(bounds.max_entries);
            break;
        }
        if n < LIST_LIMIT as usize {
            complete = true;
            break;
        }
        if entries.len() == bounds.max_entries {
            break;
        }
        match next {
            Some(c) if seen.insert(c.clone()) => cursor = Some(c),
            _ => break,
        }
    }
    if bounds.whole && !complete {
        return Err(HomeserverError::Cut { dir: dir.to_string() });
    }
    Ok(Listing { entries, complete })
}

#[derive(Clone)]
pub struct NodeHomeserver {
    pubky: Pubky,
}

/// Pubky clients built in this process: each one starts its own DHT node and keeps its own
/// homeserver cache, so a node builds one and shares it (STEWARD-FIX-pubky-signin-speed.md).
static CLIENTS_BUILT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many Pubky clients this process has built (tests check it stays at one).
pub fn clients_built() -> usize {
    CLIENTS_BUILT.load(std::sync::atomic::Ordering::Relaxed)
}

impl NodeHomeserver {
    /// A new Pubky client. A node builds one for its lifetime and clones it: clones share the
    /// connection pool, the DHT node and the homeserver cache.
    pub fn new(network: Network) -> Result<Self, HomeserverError> {
        CLIENTS_BUILT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pubky = match network {
            Network::Mainnet => Pubky::new()?,
            Network::Testnet => Pubky::testnet()?,
        };
        Ok(Self { pubky })
    }

    /// Wrap a client the caller configured (timeouts, testnet host).
    pub fn from_pubky(pubky: Pubky) -> Self {
        Self { pubky }
    }

    pub fn pubky(&self) -> &Pubky {
        &self.pubky
    }

    /// Restore a session from an exported grant secret. The secret works like
    /// a bearer token: keep it in the OS keychain, never log it.
    pub async fn restore_session(&self, secret: &str) -> Result<PubkySession, HomeserverError> {
        Ok(self.pubky.restore_session(secret).await?)
    }

    fn parse_key(z32: &str) -> Result<PublicKey, HomeserverError> {
        PublicKey::try_from(z32).map_err(|e| HomeserverError::Key(e.to_string()))
    }

    /// List a user's posts directory within `bounds`: public through anyone's view, private
    /// only with the owner's session.
    async fn list_posts(
        &self,
        user: &PublicKey,
        visibility: Visibility,
        session: Option<&PubkySession>,
        bounds: ListBounds,
    ) -> Result<Listing, HomeserverError> {
        self.list_dir(user, &visibility.posts_dir(), visibility, session, bounds)
            .await
    }

    /// The entry paths directly under `dir`, within `bounds`. A directory that does not exist
    /// yet (a fresh homeserver before the first write) lists as empty rather than failing.
    async fn list_dir(
        &self,
        user: &PublicKey,
        dir: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
        bounds: ListBounds,
    ) -> Result<Listing, HomeserverError> {
        self.list_paths(user, dir, visibility, session, true, bounds)
            .await
    }

    /// The file paths under `dir` at any depth (a v1 post tree is `{id}/{editId}.json`), within
    /// `bounds`.
    pub async fn list_tree(
        &self,
        user_z32: &str,
        dir: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
        bounds: ListBounds,
    ) -> Result<Listing, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        self.list_paths(&user, dir, visibility, session, false, bounds)
            .await
    }

    /// The newest version of every v1 post of a user under one root and namespace, within
    /// `bounds`, and whether the listing was complete. Newest first ([`ListBounds::SHARED`]) a
    /// cut listing holds the newest posts, each with its newest version first.
    pub async fn v1_heads(
        &self,
        user_z32: &str,
        root: crate::v1::Root,
        namespace: &str,
        session: Option<&PubkySession>,
        bounds: ListBounds,
    ) -> Result<(std::collections::BTreeMap<String, crate::v1::Head>, bool), HomeserverError> {
        let visibility = match root {
            crate::v1::Root::Pub => Visibility::Public,
            crate::v1::Root::Priv => Visibility::Private,
        };
        let dir = crate::v1::posts_dir(root, namespace);
        let listing = self
            .list_tree(user_z32, &dir, visibility, session, bounds)
            .await?;
        Ok((crate::v1::heads(listing.entries), listing.complete))
    }

    /// The body at `path` of a user's storage.
    pub async fn get_bytes(
        &self,
        user_z32: &str,
        path: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
    ) -> Result<Vec<u8>, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        self.fetch(&user, path, visibility, session).await
    }

    /// The signed-in user's v1 bookmarks of music URLs and public posts: `(path, uri, created_at)`.
    /// Bookmarks of anything else, and invalid ones, are skipped.
    pub async fn read_v1_bookmarks(
        &self,
        session: &PubkySession,
    ) -> Result<Vec<(String, String, i64)>, HomeserverError> {
        self.read_v1_bookmarks_except(session, &Default::default()).await
    }

    /// The same, without downloading the bookmarks the caller already knows: `known` maps a
    /// bookmark's file name to its URI and time, and a listed file of that name is taken
    /// from it. A sync then costs a listing, not one request per bookmark.
    pub async fn read_v1_bookmarks_except(
        &self,
        session: &PubkySession,
        known: &std::collections::HashMap<String, (String, i64)>,
    ) -> Result<Vec<(String, String, i64)>, HomeserverError> {
        let user = session.public_key();
        let mut out = Vec::new();
        let mut unknown = Vec::new();
        for path in self
            .list_dir(
                &user,
                &crate::v1::bookmarks_dir(),
                Visibility::Private,
                Some(session),
                ListBounds::OWNER,
            )
            .await?
            .entries
        {
            match known.get(file_name(&path)) {
                Some((uri, created_at)) => out.push((path, uri.clone(), *created_at)),
                None => unknown.push(path),
            }
        }
        for (path, bytes) in self
            .fetch_many(&user, unknown, Visibility::Private, Some(session))
            .await?
        {
            if let Some((uri, created_at)) = crate::v1::music_bookmark(&path, &bytes) {
                out.push((path, uri, created_at));
            }
        }
        Ok(out)
    }

    /// Fetch many paths [`FETCH_AT_ONCE`] at a time, results in the order of `paths`: a sync of
    /// hundreds of objects was one request after another (STEWARD-FIX-pubky-signin-speed.md).
    async fn fetch_many(
        &self,
        user: &PublicKey,
        paths: Vec<String>,
        visibility: Visibility,
        session: Option<&PubkySession>,
    ) -> Result<Vec<(String, Vec<u8>)>, HomeserverError> {
        use futures_util::StreamExt;
        let results: Vec<(String, Result<Vec<u8>, HomeserverError>)> = futures_util::stream::iter(paths)
            .map(|path| async move {
                let r = self.fetch(user, &path, visibility, session).await;
                (path, r)
            })
            .buffered(FETCH_AT_ONCE)
            .collect()
            .await;
        results.into_iter().map(|(p, r)| r.map(|b| (p, b))).collect()
    }

    async fn list_paths(
        &self,
        user: &PublicKey,
        dir: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
        shallow: bool,
        bounds: ListBounds,
    ) -> Result<Listing, HomeserverError> {
        if visibility == Visibility::Private && session.is_none() {
            return Err(HomeserverError::NeedsSession);
        }
        list_within(bounds, dir, |cursor| async move {
            let page = match (visibility, session) {
                (Visibility::Public, _) => {
                    let public = self.pubky.public_storage();
                    let mut b = public
                        .list((user, dir))?
                        .limit(LIST_LIMIT)
                        .shallow(shallow)
                        .reverse(bounds.newest_first);
                    if let Some(c) = &cursor {
                        b = b.cursor(c);
                    }
                    within(OBJECT_TIMEOUT, async { Ok(b.send().await) }).await?
                }
                (Visibility::Private, Some(session)) => {
                    let storage = session.storage();
                    let mut b = storage
                        .list(dir)?
                        .limit(LIST_LIMIT)
                        .shallow(shallow)
                        .reverse(bounds.newest_first);
                    if let Some(c) = &cursor {
                        b = b.cursor(c);
                    }
                    within(OBJECT_TIMEOUT, async { Ok(b.send().await) }).await?
                }
                (Visibility::Private, None) => return Err(HomeserverError::NeedsSession),
            };
            match page {
                Ok(p) => Ok(Some(
                    p.into_iter()
                        .map(|e| (e.path.to_string(), e.to_pubky_url()))
                        .collect(),
                )),
                Err(e) if is_not_found(&e) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn fetch(
        &self,
        user: &PublicKey,
        path: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
    ) -> Result<Vec<u8>, HomeserverError> {
        within(OBJECT_TIMEOUT, async {
            let resp = match (visibility, session) {
                (Visibility::Public, _) => self.pubky.public_storage().get((user, path)).await?,
                (Visibility::Private, Some(session)) => session.storage().get(path).await?,
                (Visibility::Private, None) => return Err(HomeserverError::NeedsSession),
            };
            read_capped(resp, OBJECT_MAX, STALL_TIMEOUT).await
        })
        .await
    }

    /// Every music post of a user: `(path, post)`. Ordinary posts are
    /// skipped silently; invalid music envelopes are skipped with a warning
    /// (an indexer never trusts a homeserver body). Private posts need the
    /// owner's session.
    pub async fn read_music_posts(
        &self,
        user_z32: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
        bounds: ListBounds,
    ) -> Result<Vec<(String, MusicPost)>, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        let mut out = Vec::new();
        let paths = self.list_posts(&user, visibility, session, bounds).await?.entries;
        for (path, bytes) in self.fetch_many(&user, paths, visibility, session).await? {
            let id = path.rsplit('/').next().unwrap_or("").to_string();
            match MusicPost::try_from_bytes(&bytes, &id) {
                Ok(Some(post)) => out.push((path, post)),
                Ok(None) => {}
                Err(source) => tracing::warn!(%path, %source, "skipping invalid music post"),
            }
        }
        Ok(out)
    }

    /// The user's older `pubky.app` bookmarks of music URLs under `visibility`; other
    /// bookmarks (of posts) are skipped.
    pub async fn read_legacy_bookmarks(
        &self,
        user_z32: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
    ) -> Result<Vec<(String, Bookmark)>, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        let dir = Bookmark::dir(visibility);
        let mut out = Vec::new();
        let paths = self
            .list_dir(&user, &dir, visibility, session, ListBounds::OWNER)
            .await?
            .entries;
        for (path, bytes) in self.fetch_many(&user, paths, visibility, session).await? {
            let id = path.rsplit('/').next().unwrap_or("").to_string();
            match serde_json::from_slice::<pubky_app_specs::PubkyAppBookmark>(&bytes) {
                Ok(b) => {
                    if let Some(f) = Bookmark::from_bookmark(&id, &b) {
                        out.push((path, f));
                    }
                }
                Err(e) => tracing::warn!(%path, error = %e, "skipping invalid bookmark"),
            }
        }
        Ok(out)
    }

    /// A user's public tags on music URLs; other tags skipped. Reads `/pub/social/v1/tags/`
    /// and the two older folders.
    pub async fn read_music_tags(&self, user_z32: &str) -> Result<RemoteTags, HomeserverError> {
        self.read_music_tags_in(user_z32, &Default::default(), true, true)
            .await
    }

    /// The music tags whose id is not in `known` (the ones the caller has, or knows are not
    /// music): listed, never downloaded again. Reads `/pub/social/v1/tags/`; with `app_dir` also
    /// `/pub/pubky.music/tags/`, and with `legacy_dir` `/pub/pubky.app/tags/`: the folders the
    /// node wrote to before, until it has moved its tags out (the legacy one holds other apps'
    /// tags too: hundreds for an active user).
    pub async fn read_music_tags_in(
        &self,
        user_z32: &str,
        known: &std::collections::HashSet<String>,
        app_dir: bool,
        legacy_dir: bool,
    ) -> Result<RemoteTags, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        let mut out = RemoteTags::default();
        let v1_paths: Vec<String> = self
            .list_dir(
                &user,
                &crate::v1::tags_dir(),
                Visibility::Public,
                None,
                ListBounds::OWNER,
            )
            .await?
            .entries
            .into_iter()
            .filter(|p| crate::v1::tag_id_of(p).is_some_and(|id| !known.contains(id)))
            .collect();
        for (path, bytes) in self.fetch_many(&user, v1_paths, Visibility::Public, None).await? {
            match crate::v1::music_tag_from(&path, &bytes) {
                Some((id, t)) => out.tags.push(RemoteTag {
                    path,
                    id,
                    uri: t.uri,
                    label: t.label,
                    created_at: t.created_at,
                }),
                // Another app's tag (on a post, a link): never fetched again
                None => out
                    .not_music
                    .extend(crate::v1::tag_id_of(&path).map(str::to_string)),
            }
        }
        let mut old = Vec::new();
        if app_dir {
            old.extend(
                self.list_dir(
                    &user,
                    crate::schemas::TAG_DIR,
                    Visibility::Public,
                    None,
                    ListBounds::OWNER,
                )
                .await
                .map(|l| l.entries)
                .unwrap_or_default(),
            );
        }
        if legacy_dir {
            old.extend(
                self.list_dir(
                    &user,
                    crate::schemas::LEGACY_TAG_DIR,
                    Visibility::Public,
                    None,
                    ListBounds::OWNER,
                )
                .await?
                .entries,
            );
        }
        old.retain(|p| !known.contains(file_name(p)));
        for (path, bytes) in self.fetch_many(&user, old, Visibility::Public, None).await? {
            match serde_json::from_slice::<pubky_app_specs::PubkyAppTag>(&bytes) {
                Ok(t) if pm_model::MusicUri::parse(&t.uri).is_ok() => out.tags.push(RemoteTag {
                    id: file_name(&path).to_string(),
                    path,
                    uri: t.uri,
                    label: t.label,
                    created_at: t.created_at,
                }),
                Ok(_) => {}
                Err(e) => tracing::warn!(%path, error = %e, "skipping invalid tag"),
            }
        }
        Ok(out)
    }

    /// The public paths under `dir` of the owner (`/pub/...`), e.g. to check where tags live
    /// ([`ListBounds::OWNER`]).
    pub async fn public_paths(&self, user_z32: &str, dir: &str) -> Result<Vec<String>, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        Ok(self
            .list_dir(&user, dir, Visibility::Public, None, ListBounds::OWNER)
            .await?
            .entries)
    }

    /// Move the user's tags on external links out of `dir` (an older folder of `PubkyAppTag`s:
    /// `/pub/pubky.app/tags/` or `/pub/pubky.music/tags/`) to social v1 tags in
    /// `/pub/social/v1/tags/` (same id, uri, label and time). Tags on `pubky://` objects, and
    /// ones v1 refuses, stay. Safe to run again: a tag already moved is written again and
    /// deleted at the old path. Returns (moved, kept).
    pub async fn move_tags_to_v1(
        &self,
        session: &PubkySession,
        dir: &str,
    ) -> Result<(usize, usize), HomeserverError> {
        let user = session.public_key();
        let paths = self
            .list_dir(&user, dir, Visibility::Public, None, ListBounds::OWNER)
            .await?
            .entries;
        let (mut moved, mut kept) = (0, 0);
        // The legacy folder also holds other apps' tags: read 8 at a time (a node runs this once)
        for (path, bytes) in self.fetch_many(&user, paths, Visibility::Public, None).await? {
            let Ok(tag) = serde_json::from_slice::<pubky_app_specs::PubkyAppTag>(&bytes) else {
                kept += 1;
                continue;
            };
            if tag.uri.starts_with("pubky://") {
                kept += 1;
                continue;
            }
            let Ok((v1_tag, _, v1_path)) = crate::v1::music_tag(&tag.uri, &tag.label, tag.created_at) else {
                tracing::warn!(%path, "a tag social v1 refuses stays where it is");
                kept += 1;
                continue;
            };
            self.write_object(session, &v1_path, &v1_tag).await?;
            self.delete(session, &path).await?;
            moved += 1;
        }
        Ok((moved, kept))
    }

    /// Another user's public playlists.
    pub async fn read_playlists_of(
        &self,
        user_z32: &str,
    ) -> Result<Vec<(String, Playlist)>, HomeserverError> {
        Ok(self
            .read_music_posts(user_z32, Visibility::Public, None, ListBounds::SHARED)
            .await?
            .into_iter()
            .filter_map(|(p, post)| post.as_playlist().cloned().map(|pl| (p, pl)))
            .collect())
    }

    /// The signed-in user's private playlists.
    pub async fn read_private_playlists(
        &self,
        session: &PubkySession,
    ) -> Result<Vec<(String, Playlist)>, HomeserverError> {
        let me = session.public_key().to_string();
        Ok(self
            .read_music_posts(&me, Visibility::Private, Some(session), ListBounds::OWNER)
            .await?
            .into_iter()
            .filter_map(|(p, post)| post.as_playlist().cloned().map(|pl| (p, pl)))
            .collect())
    }

    pub async fn read_reviews_of(&self, user_z32: &str) -> Result<Vec<(String, Review)>, HomeserverError> {
        Ok(self
            .read_music_posts(user_z32, Visibility::Public, None, ListBounds::SHARED)
            .await?
            .into_iter()
            .filter_map(|(p, post)| post.as_review().cloned().map(|r| (p, r)))
            .collect())
    }

    pub async fn read_activity_of(&self, user_z32: &str) -> Result<Vec<(String, Activity)>, HomeserverError> {
        Ok(self
            .read_music_posts(user_z32, Visibility::Public, None, ListBounds::SHARED)
            .await?
            .into_iter()
            .filter_map(|(p, post)| post.as_activity().cloned().map(|a| (p, a)))
            .collect())
    }

    /// Write any object to its session-relative path (`Meta::path`).
    pub async fn write_object<T: Serialize + Sync + ?Sized>(
        &self,
        session: &PubkySession,
        path: &str,
        object: &T,
    ) -> Result<(), HomeserverError> {
        session.storage().put_json(path, object).await?;
        Ok(())
    }

    /// Write a music post where its `Meta` says (private or public folder).
    pub async fn write_post(
        &self,
        session: &PubkySession,
        post: &MusicPost,
        meta: &crate::schemas::Meta,
    ) -> Result<(), HomeserverError> {
        self.write_object(session, &meta.path, &post.post).await
    }

    /// Delete an object the user wrote (remove a playlist, retract a review).
    pub async fn delete(&self, session: &PubkySession, path: &str) -> Result<(), HomeserverError> {
        session.storage().delete(path).await?;
        Ok(())
    }

    // ---- private files (audio the user keeps on their homeserver) ----

    /// Upload bytes to a private path (`/priv/…`). Audio never goes under
    /// `/pub/` (constraint 2); this refuses public paths outright.
    pub async fn upload_private(
        &self,
        session: &PubkySession,
        path: &str,
        bytes: Vec<u8>,
    ) -> Result<(), HomeserverError> {
        if !path.starts_with(PRIVATE_FILES_DIR) {
            return Err(HomeserverError::Transport(format!(
                "refusing to upload audio outside {PRIVATE_FILES_DIR}: {path}"
            )));
        }
        session.storage().put(path, bytes).await?;
        Ok(())
    }

    /// The same, sent no faster than `bytes_per_sec` (`None`: as fast as the line allows). The
    /// node's uplink also carries its pages and the music it streams to a browser away from home:
    /// a copy at full speed starved both (owner, 2026-10-22).
    pub async fn upload_private_paced(
        &self,
        session: &PubkySession,
        path: &str,
        bytes: Vec<u8>,
        bytes_per_sec: Option<u64>,
    ) -> Result<(), HomeserverError> {
        let Some(rate) = bytes_per_sec.filter(|r| *r > 0) else {
            return self.upload_private(session, path, bytes).await;
        };
        if !path.starts_with(PRIVATE_FILES_DIR) {
            return Err(HomeserverError::Transport(format!(
                "refusing to upload audio outside {PRIVATE_FILES_DIR}: {path}"
            )));
        }
        let len = bytes.len() as u64;
        let data = bytes::Bytes::from(bytes);
        let start = tokio::time::Instant::now();
        const CHUNK: usize = 64 * 1024;
        let stream = futures_util::stream::unfold(0usize, move |sent| {
            let data = data.clone();
            async move {
                if sent >= data.len() {
                    return None;
                }
                // Wait until this chunk is due at the chosen rate
                let due = start + std::time::Duration::from_secs_f64(sent as f64 / rate as f64);
                tokio::time::sleep_until(due).await;
                let end = (sent + CHUNK).min(data.len());
                Some((Ok::<_, std::io::Error>(data.slice(sent..end)), end))
            }
        });
        let body = reqwest::Body::wrap_stream(stream);
        tracing::debug!(path, len, rate, "paced upload");
        session.storage().put(path, body).await?;
        Ok(())
    }

    /// Check authenticated private storage with one bounded listing, not a file inventory.
    /// A new account's missing directory is healthy; access and transport errors propagate.
    pub async fn probe_private_storage(&self, session: &PubkySession) -> Result<(), HomeserverError> {
        let storage = session.storage();
        match storage
            .list(PRIVATE_FILES_DIR)?
            .limit(1)
            .shallow(true)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Every file under the private files folder, with size when the homeserver reports it
    /// ([`ListBounds::OWNER`]: complete, or an error).
    pub async fn list_private_files(
        &self,
        session: &PubkySession,
    ) -> Result<Vec<RemoteFile>, HomeserverError> {
        self.list_private_files_within(session, ListBounds::OWNER).await
    }

    /// [`NodeHomeserver::list_private_files`] within `bounds`, which cover the listing's pages;
    /// the size of each file is then read [`FETCH_AT_ONCE`] at a time, each read within
    /// [`OBJECT_TIMEOUT`] (a size that can't be read is `None`), so a large folder on a slow
    /// homeserver lists in the time its pages take, not one round trip per file.
    pub async fn list_private_files_within(
        &self,
        session: &PubkySession,
        bounds: ListBounds,
    ) -> Result<Vec<RemoteFile>, HomeserverError> {
        use futures_util::StreamExt;
        let storage = session.storage();
        let listing = list_within(bounds, PRIVATE_FILES_DIR, |cursor| {
            let storage = &storage;
            async move {
                let mut b = storage.list(PRIVATE_FILES_DIR)?.limit(LIST_LIMIT).shallow(true);
                if let Some(c) = &cursor {
                    b = b.cursor(c);
                }
                match within(OBJECT_TIMEOUT, async { Ok(b.send().await) }).await? {
                    Ok(p) => Ok(Some(
                        p.into_iter()
                            .map(|e| (e.path.to_string(), e.to_pubky_url()))
                            .collect(),
                    )),
                    Err(e) if is_not_found(&e) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            }
        })
        .await?;
        let storage = &storage;
        Ok(futures_util::stream::iter(listing.entries)
            .map(|path| async move {
                let size = within(OBJECT_TIMEOUT, async { Ok(storage.stats(path.as_str()).await) })
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .flatten()
                    .and_then(|s| s.content_length);
                RemoteFile { path, size }
            })
            .buffered(FETCH_AT_ONCE)
            .collect()
            .await)
    }

    /// The size of one private file, as the homeserver reports it (`None`: not there; `Some(None)`:
    /// there, its size not given).
    pub async fn stat_private(
        &self,
        session: &PubkySession,
        path: &str,
    ) -> Result<Option<Option<u64>>, HomeserverError> {
        Ok(session.storage().stats(path).await?.map(|s| s.content_length))
    }

    /// Remove one private file (only under the files folder).
    pub async fn delete_private(&self, session: &PubkySession, path: &str) -> Result<(), HomeserverError> {
        if !path.starts_with(PRIVATE_FILES_DIR) {
            return Err(HomeserverError::Transport(format!(
                "refusing to delete outside {PRIVATE_FILES_DIR}: {path}"
            )));
        }
        session.storage().delete(path).await?;
        Ok(())
    }

    /// Download a private file into `dest` as it arrives (no whole file in memory), no faster
    /// than `bytes_per_sec` when set; `progress` hears each chunk's size and returns `false` to
    /// stop (the partial file is removed). Returns the bytes written.
    pub async fn download_private_to(
        &self,
        session: &PubkySession,
        path: &str,
        dest: &std::path::Path,
        bytes_per_sec: Option<u64>,
        mut progress: impl FnMut(u64) -> bool,
    ) -> Result<u64, HomeserverError> {
        use tokio::io::AsyncWriteExt;
        let io = |e: std::io::Error| HomeserverError::Transport(format!("{}: {e}", dest.display()));
        let mut resp = within(STALL_TIMEOUT, async { Ok(session.storage().get(path).await?) }).await?;
        let mut file = tokio::fs::File::create(dest).await.map_err(io)?;
        let start = tokio::time::Instant::now();
        let mut written: u64 = 0;
        // No limit on the whole download (a big file at a paced rate takes long), only on a stall
        while let Some(chunk) = within(STALL_TIMEOUT, async {
            resp.chunk()
                .await
                .map_err(|e| HomeserverError::Transport(e.to_string()))
        })
        .await?
        {
            file.write_all(&chunk).await.map_err(io)?;
            written += chunk.len() as u64;
            if !progress(chunk.len() as u64) {
                drop(file);
                let _ = tokio::fs::remove_file(dest).await;
                return Err(HomeserverError::Transport("cancelled".into()));
            }
            if let Some(rate) = bytes_per_sec.filter(|r| *r > 0) {
                let due = start + std::time::Duration::from_secs_f64(written as f64 / rate as f64);
                tokio::time::sleep_until(due).await;
            }
        }
        file.flush().await.map_err(io)?;
        file.sync_all().await.map_err(io)?;
        Ok(written)
    }

    /// A private file whole in memory, at most [`FILE_MAX`]; music files go to disk with
    /// [`Self::download_private_to`].
    pub async fn download_private(
        &self,
        session: &PubkySession,
        path: &str,
    ) -> Result<Vec<u8>, HomeserverError> {
        let resp = within(STALL_TIMEOUT, async { Ok(session.storage().get(path).await?) }).await?;
        read_capped(resp, FILE_MAX, STALL_TIMEOUT).await
    }
}

/// Where the user's own audio files live on the homeserver: private, never `/pub/`.
pub const PRIVATE_FILES_DIR: &str = pm_appdata::paths::FILES_DIR;
/// Where a node publishes how to reach it, readable only by the owner (ADR 0008).
pub const NODE_RECORD_PATH: &str = pm_appdata::paths::NODE_RECORD_PATH;
/// The node's settings (contact, folders, renderers, DSP, library and Home choices), so a
/// reinstalled or replaced node restores them after one Ring sign-in. Never a sign-in or a token:
/// the file is plain JSON the homeserver's operator can read.
pub const NODE_SETTINGS_PATH: &str = pm_appdata::paths::NODE_SETTINGS_PATH;
/// The person's own preferences ("You" in Settings), read and written by every node they use.
pub const PREFS_PATH: &str = pm_appdata::paths::PREFS_PATH;

/// How the user's own devices find their node (ADR 0008).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeRecord {
    pub name: String,
    /// HTTP(S) origin of the `/v1` API as the user's devices can reach it.
    pub api_url: String,
    /// Never written: the record says where the node is, not how to sign in to it. A device gets
    /// its own credential by signing in to the node with Pubky Ring (ADR 0008). Read only to
    /// recognise a record an older version wrote with the node's API token in it, so the node can
    /// replace that token.
    #[serde(default, skip_serializing)]
    pub token: String,
    pub version: String,
    /// Microseconds.
    pub updated_at: i64,
    /// The node's iroh key when remote access is on (ADR 0013): dial it from anywhere.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub iroh: String,
}

impl NodeHomeserver {
    pub async fn publish_node_record(
        &self,
        session: &PubkySession,
        record: &NodeRecord,
    ) -> Result<(), HomeserverError> {
        self.write_object(session, NODE_RECORD_PATH, record).await
    }

    /// The owner's node settings backup, written by the node after every sync.
    pub async fn write_node_settings(
        &self,
        session: &PubkySession,
        settings: &serde_json::Value,
    ) -> Result<(), HomeserverError> {
        self.write_object(session, NODE_SETTINGS_PATH, settings).await
    }

    pub async fn write_prefs(
        &self,
        session: &PubkySession,
        prefs: &serde_json::Value,
    ) -> Result<(), HomeserverError> {
        self.write_object(session, PREFS_PATH, prefs).await
    }

    pub async fn read_prefs(
        &self,
        session: &PubkySession,
    ) -> Result<Option<serde_json::Value>, HomeserverError> {
        // One request: a 404 means there are none yet
        match session.storage().get_json(PREFS_PATH).await {
            Ok(v) => Ok(Some(v)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn read_node_settings(
        &self,
        session: &PubkySession,
    ) -> Result<Option<serde_json::Value>, HomeserverError> {
        let storage = session.storage();
        if !storage.exists(NODE_SETTINGS_PATH).await? {
            return Ok(None);
        }
        let v: serde_json::Value = storage.get_json(NODE_SETTINGS_PATH).await?;
        Ok(Some(v))
    }

    /// The owner's node record, if a node published one.
    pub async fn read_node_record(
        &self,
        session: &PubkySession,
    ) -> Result<Option<NodeRecord>, HomeserverError> {
        let storage = session.storage();
        if !storage.exists(NODE_RECORD_PATH).await? {
            return Ok(None);
        }
        let record: NodeRecord = storage.get_json(NODE_RECORD_PATH).await?;
        Ok(Some(record))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteFile {
    /// Owner-relative path, e.g. `/priv/pubky.music/files/<hash>-<name>`.
    pub path: String,
    pub size: Option<u64>,
}

impl RemoteFile {
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// Sign-in through Pubky Ring (M9): start a grant auth flow, show the URL
/// (or a QR of it) to the user, await approval, keep the exported secret.
pub struct LoginFlow {
    flow: pubky::PubkyGrantAuthFlow,
    url: String,
}

/// What the node asks for: its own private folders, pubky.app objects, its
/// public tag namespace (`/pub/pubky.music/`, where Nexus indexes tags on
/// external links today), and the v1 social and music namespaces it moves to
/// (`social/v1`, `music/v1`; STEWARD-PLAN step 3), so one approval covers both.
///
/// The app's private folder is asked for by its parts (the node record, the settings backup, the
/// person's preferences, their files, the sources vault), never as `/priv/pubky.music/` itself:
/// pubky's canonical form drops a capability that a broader one covers, so a signer or a
/// homeserver that normalises would lose the vault capability, and the vault counts only when it
/// is named itself. A session from before, with the whole folder, still covers every part.
pub const LOGIN_CAPABILITIES: &str = "/priv/pubky.music/vault/:rw,/priv/pubky.music/node.json:rw,/priv/pubky.music/settings.json:rw,/priv/pubky.music/prefs.json:rw,/priv/pubky.music/files/:rw,/pub/pubky.music/:rw,/priv/pubky.app/:rw,/pub/pubky.app/:rw,/pub/social/v1/:rw,/priv/social/v1/:rw,/pub/music/v1/:rw,/priv/music/v1/:rw";

/// The capabilities in `wanted` (a list like [`LOGIN_CAPABILITIES`]) that `granted` does not
/// cover. A granted `scope:actions` covers a wanted one when its scope is the same folder or a
/// parent of it and it has every wanted action.
pub fn missing_capabilities<S: AsRef<str>>(granted: &[S], wanted: &str) -> Vec<String> {
    let split = |c: &str| -> Option<(String, String)> {
        let (scope, actions) = c.trim().rsplit_once(':')?;
        Some((scope.to_string(), actions.to_string()))
    };
    let have: Vec<(String, String)> = granted.iter().filter_map(|g| split(g.as_ref())).collect();
    wanted
        .split(',')
        .filter(|w| !w.trim().is_empty())
        .filter(|w| {
            let Some((scope, actions)) = split(w) else {
                return false;
            };
            // The sources vault counts only when named itself (VAULT.md): a wider grant doesn't
            // give a client the person's streaming sign-ins
            let exact_only = scope == pm_appdata::vault::VAULT_DIR;
            !have.iter().any(|(hs, ha)| {
                let parent =
                    !exact_only && (hs == "/" || (hs.ends_with('/') && scope.starts_with(hs.as_str())));
                (parent || *hs == scope) && actions.chars().all(|a| ha.contains(a))
            })
        })
        .map(|w| w.trim().to_string())
        .collect()
}

/// The capabilities this session was granted, as `scope:actions` strings.
pub fn session_capabilities(session: &PubkySession) -> Vec<String> {
    session
        .info()
        .capabilities()
        .iter()
        .map(|c| c.to_string())
        .collect()
}

/// Whether a homeserver error says the session lacks access to a path (a session approved for
/// fewer folders than the node now uses).
pub fn is_capability_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("does not have") && lower.contains("access")
        || lower.contains("403")
        || lower.contains("forbidden")
}

/// What a controller asks for: read access to the user's public pubky.app
/// data only, enough to prove who is signing in. The node never keeps this
/// session.
pub const CONTROLLER_CAPABILITIES: &str = "/pub/pubky.app/:r";

impl NodeHomeserver {
    pub fn start_login(&self, client_id: &str) -> Result<LoginFlow, HomeserverError> {
        self.start_login_with(client_id, LOGIN_CAPABILITIES)
    }

    /// A sign-in asking for `capabilities` (see [`CONTROLLER_CAPABILITIES`]).
    pub fn start_login_with(
        &self,
        client_id: &str,
        capabilities: &str,
    ) -> Result<LoginFlow, HomeserverError> {
        let caps = capabilities
            .parse::<pubky::Capabilities>()
            .map_err(|e| HomeserverError::Transport(e.to_string()))?;
        let client_id =
            pubky::ClientId::new(client_id).map_err(|e| HomeserverError::Transport(e.to_string()))?;
        let flow = self
            .pubky
            .start_grant_auth_flow(&caps, pubky::AuthFlowKind::signin(), client_id)?;
        let url = flow.authorization_url().to_string();
        Ok(LoginFlow { flow, url })
    }

    /// The pending sign-in `flow`, listening again on its relay inbox through this homeserver's
    /// client: Ring's approval waits in the inbox until it is read, so a relay poll that gave up
    /// (the app lost the network in the background) can pick it up. No grant has been sent then.
    pub fn resume_login(&self, flow: &LoginFlow) -> Result<LoginFlow, HomeserverError> {
        let state = flow
            .flow
            .save_local()
            .ok_or_else(|| HomeserverError::Transport("this sign-in cannot be resumed".into()))?;
        let resumed = pubky::PubkyGrantAuthFlow::restore(state, self.pubky.client().clone())?;
        Ok(LoginFlow {
            flow: resumed,
            url: flow.url.clone(),
        })
    }

    /// A sign-up on `homeserver` (z32) with `signup_token`: Pubky Ring creates the account there,
    /// then approves the node's [`LOGIN_CAPABILITIES`]; poll it like a sign-in.
    pub fn start_signup(
        &self,
        client_id: &str,
        homeserver: &str,
        signup_token: &str,
    ) -> Result<LoginFlow, HomeserverError> {
        let caps = LOGIN_CAPABILITIES
            .parse::<pubky::Capabilities>()
            .map_err(|e| HomeserverError::Transport(e.to_string()))?;
        let client_id =
            pubky::ClientId::new(client_id).map_err(|e| HomeserverError::Transport(e.to_string()))?;
        let hs = PublicKey::try_from(homeserver).map_err(|e| HomeserverError::Key(e.to_string()))?;
        let kind = pubky::AuthFlowKind::signup(hs, Some(signup_token.to_string()));
        let flow = self.pubky.start_grant_auth_flow(&caps, kind, client_id)?;
        let url = flow.authorization_url().to_string();
        Ok(LoginFlow { flow, url })
    }
}

impl LoginFlow {
    /// The `pubkyauth://` URL for Pubky Ring (scan or paste).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// One poll: `Ok(None)` while the user has not approved yet. It must not wait for anything
    /// before pubky's `try_poll_once`: the app tells a relay listener's error (returned at once)
    /// from the grant exchange's by that (pm-desktop `ring_wait::at_once`).
    pub async fn poll(&self) -> Result<Option<PubkySession>, HomeserverError> {
        Ok(self.flow.try_poll_once().await?)
    }

    /// Block until approved.
    pub async fn await_approval(self) -> Result<PubkySession, HomeserverError> {
        Ok(self.flow.await_approval().await?)
    }
}

/// The restorable secret of a session (grant or legacy cookie), for
/// `pubky.session_secret_file`.
#[allow(deprecated)]
pub async fn export_session_secret(session: &PubkySession) -> Option<String> {
    if let Some(g) = session.as_grant() {
        if let Some(s) = g.export_local_secret().await {
            return Some(s);
        }
    }
    session.as_cookie().and_then(|c| c.export_secret())
}

/// The last segment of a path (a file name, which for tags and bookmarks is the object's id).
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    #[test]
    fn an_old_session_misses_the_new_folders() {
        let old = [
            "/priv/pubky.music/:rw",
            "/pub/pubky.app/:rw",
            "/priv/pubky.app/:rw",
        ];
        let missing = missing_capabilities(&old, LOGIN_CAPABILITIES);
        assert!(missing.contains(&"/pub/social/v1/:rw".to_string()));
        assert!(missing.contains(&"/pub/pubky.music/:rw".to_string()));
        assert!(!missing.contains(&"/priv/pubky.app/:rw".to_string()));
        let full: Vec<&str> = LOGIN_CAPABILITIES.split(',').collect();
        assert!(missing_capabilities(&full, LOGIN_CAPABILITIES).is_empty());
        // Everything but the sources vault, which must be named itself (VAULT.md)
        assert_eq!(
            missing_capabilities(&["/:rw"], LOGIN_CAPABILITIES),
            vec!["/priv/pubky.music/vault/:rw".to_string()]
        );
        assert!(
            missing.contains(&"/priv/pubky.music/vault/:rw".to_string()),
            "a session from before the vault asks again"
        );
        // A session approved with the set from before the parts still covers today's set
        let before: Vec<&str> = "/priv/pubky.music/vault/:rw,/priv/pubky.music/:rw,/pub/pubky.music/:rw,/priv/pubky.app/:rw,/pub/pubky.app/:rw,/pub/social/v1/:rw,/priv/social/v1/:rw,/pub/music/v1/:rw,/priv/music/v1/:rw"
            .split(',')
            .collect();
        assert_eq!(
            missing_capabilities(&before, LOGIN_CAPABILITIES),
            Vec::<String>::new()
        );
        // Normalising the set changes nothing (no entry covers another), so a signer or a
        // homeserver that normalises can't drop the vault or anything else
        let caps: pubky::Capabilities = LOGIN_CAPABILITIES.parse().unwrap();
        assert_eq!(caps.normalize().to_string(), LOGIN_CAPABILITIES);
        assert_eq!(
            missing_capabilities(&["/pub/social/v1/:r"], "/pub/social/v1/:rw"),
            vec!["/pub/social/v1/:rw".to_string()],
            "read-only does not cover write"
        );
        assert!(is_capability_error(
            "Server responded with an error: 403 Forbidden - Session does not have read access to path"
        ));
        assert!(!is_capability_error("connection reset"));
    }
}

#[cfg(test)]
mod full_tests {
    use super::*;

    #[test]
    fn a_507_is_a_full_homeserver_and_nothing_else_is() {
        let server = |status: u16| {
            HomeserverError::Pubky(pubky::Error::Request(pubky::errors::RequestError::Server {
                status: reqwest::StatusCode::from_u16(status).unwrap(),
                message: String::new(),
            }))
        };
        assert!(is_full(&server(507)));
        assert!(!is_full(&server(404)));
        assert!(!is_full(&server(413)));
        assert!(!is_full(&HomeserverError::Transport("reset".into())));
    }
}

#[cfg(test)]
mod listing_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn bounds(max_entries: usize, max_pages: usize, deadline_ms: u64) -> ListBounds {
        ListBounds {
            max_entries,
            max_pages,
            deadline: std::time::Duration::from_millis(deadline_ms),
            newest_first: false,
            whole: false,
        }
    }

    /// A directory of `total` entries as a homeserver pages it (`LIST_LIMIT` a page, the cursor
    /// the last entry's URL), counting the pages asked for; `stall_from` makes that page and
    /// the later ones never answer.
    async fn list(
        total: usize,
        stall_from: usize,
        b: ListBounds,
        asked: &AtomicUsize,
    ) -> Result<Listing, HomeserverError> {
        list_within(b, "/pub/x/", |cursor| async move {
            if asked.fetch_add(1, Ordering::SeqCst) + 1 >= stall_from {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
            let from = cursor.map_or(0, |c| c.rsplit('/').next().unwrap().parse::<usize>().unwrap() + 1);
            let to = total.min(from + LIST_LIMIT as usize);
            Ok(Some(
                (from..to)
                    .map(|i| (format!("/pub/x/{i:07}"), format!("pubky://u/pub/x/{i}")))
                    .collect(),
            ))
        })
        .await
    }

    #[tokio::test]
    async fn a_directory_within_the_bounds_lists_whole() {
        let asked = AtomicUsize::new(0);
        let l = list(250, usize::MAX, bounds(10_000, 100, 5_000), &asked)
            .await
            .unwrap();
        assert_eq!((l.entries.len(), l.complete), (250, true));
        assert_eq!(asked.load(Ordering::SeqCst), 3);
        assert_eq!(l.entries[249], "/pub/x/0000249");
        // Exactly full pages end with one more, empty page
        let asked = AtomicUsize::new(0);
        let l = list(200, usize::MAX, bounds(10_000, 100, 5_000), &asked)
            .await
            .unwrap();
        assert_eq!((l.entries.len(), l.complete), (200, true));
        assert_eq!(asked.load(Ordering::SeqCst), 3);
        // A directory that isn't there lists as empty, complete
        let l = list_within(bounds(10, 10, 1_000), "/pub/none/", |_| async {
            Ok(None::<Page<String>>)
        })
        .await
        .unwrap();
        assert_eq!(
            l,
            Listing {
                entries: vec![],
                complete: true
            }
        );
    }

    #[tokio::test]
    async fn an_endless_directory_stops_at_the_page_cap() {
        let asked = AtomicUsize::new(0);
        let l = list(usize::MAX / 2, usize::MAX, bounds(1_000_000, 5, 5_000), &asked)
            .await
            .unwrap();
        assert_eq!((l.entries.len(), l.complete), (500, false));
        assert_eq!(asked.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn an_endless_directory_stops_at_the_entry_cap() {
        // The page that crosses the cap is cut
        let asked = AtomicUsize::new(0);
        let l = list(usize::MAX / 2, usize::MAX, bounds(250, 100, 5_000), &asked)
            .await
            .unwrap();
        assert_eq!((l.entries.len(), l.complete), (250, false));
        assert_eq!(asked.load(Ordering::SeqCst), 3);
        // A cap on a page edge asks for no page past it, and is not taken as the end
        let asked = AtomicUsize::new(0);
        let l = list(usize::MAX / 2, usize::MAX, bounds(200, 100, 5_000), &asked)
            .await
            .unwrap();
        assert_eq!((l.entries.len(), l.complete), (200, false));
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_cursor_that_doesnt_move_on_stops_the_listing() {
        let asked = AtomicUsize::new(0);
        let same_page = |_: Option<String>| async {
            asked.fetch_add(1, Ordering::SeqCst);
            Ok(Some(
                (0..LIST_LIMIT as usize)
                    .map(|i| (format!("/pub/x/{i}"), "pubky://u/pub/x/99".to_string()))
                    .collect::<Page<String>>(),
            ))
        };
        let l = list_within(bounds(10_000, 100, 5_000), "/pub/x/", same_page)
            .await
            .unwrap();
        assert!(!l.complete);
        assert_eq!(
            asked.load(Ordering::SeqCst),
            2,
            "the page after the repeated cursor is never asked for"
        );
    }

    #[tokio::test]
    async fn a_homeserver_that_stalls_hits_the_deadline() {
        let asked = AtomicUsize::new(0);
        let started = std::time::Instant::now();
        let l = list(usize::MAX / 2, 3, bounds(10_000, 100, 400), &asked)
            .await
            .unwrap();
        assert_eq!((l.entries.len(), l.complete), (200, false));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(asked.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_listing_that_must_be_whole_is_an_error_when_cut() {
        let asked = AtomicUsize::new(0);
        let whole = ListBounds {
            whole: true,
            ..bounds(150, 100, 5_000)
        };
        let e = list(usize::MAX / 2, usize::MAX, whole, &asked).await.unwrap_err();
        assert!(
            matches!(&e, HomeserverError::Cut { dir } if dir == "/pub/x/"),
            "{e}"
        );
        let asked = AtomicUsize::new(0);
        let l = list(120, usize::MAX, whole, &asked).await.unwrap();
        assert_eq!((l.entries.len(), l.complete), (120, true));
    }
}

#[cfg(test)]
mod object_tests {
    use super::*;
    use std::io::{Read, Write};

    /// A server answering each connection with the next of `bodies` (`(with_length, size,
    /// stall)`): `size` bytes, its length said or not, or after the head nothing at all for a
    /// stall. Returns its URL and how many body bytes each answer got out.
    fn serve(answers: Vec<(bool, usize, bool)>) -> (String, std::thread::JoinHandle<Vec<usize>>) {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/pub/x.json", server.local_addr().unwrap());
        let t = std::thread::spawn(move || {
            let mut sent_all = Vec::new();
            for (with_length, size, stall) in answers {
                let (mut c, _) = server.accept().unwrap();
                let mut req = [0u8; 2048];
                let _ = c.read(&mut req);
                let len = if with_length {
                    format!("Content-Length: {size}\r\n")
                } else {
                    String::new()
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{len}Connection: close\r\n\r\n"
                );
                c.write_all(head.as_bytes()).unwrap();
                if stall {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    sent_all.push(0);
                    continue;
                }
                let block = vec![b' '; 64 * 1024];
                let mut sent = 0;
                while sent < size {
                    match c.write(&block[..block.len().min(size - sent)]) {
                        Ok(n) if n > 0 => sent += n,
                        _ => break,
                    }
                }
                sent_all.push(sent);
            }
            sent_all
        });
        (url, t)
    }

    /// Someone else's homeserver can answer a post read with as much as it likes: an object over
    /// [`OBJECT_MAX`] is refused unread when it says its size and given up past the cap when it
    /// doesn't, and one that stops sending is given up (security review 2026-11-02, medium 3).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_homeserver_object_is_read_to_a_cap_and_a_stall_is_given_up() {
        const HUGE: usize = 64 * 1024 * 1024;
        let (url, server) = serve(vec![
            (true, 512 * 1024, false),
            (true, HUGE, false),
            (false, HUGE, false),
            (false, 10, true),
        ]);
        let client = reqwest::Client::new();
        let get = || async { client.get(&url).send().await.unwrap() };
        let stall = std::time::Duration::from_millis(300);

        // A post as big as social v1 allows reads whole
        let ok = read_capped(get().await, OBJECT_MAX, stall).await.unwrap();
        assert_eq!(ok.len(), 512 * 1024);
        // Too big, said or not
        for _ in 0..2 {
            let e = read_capped(get().await, OBJECT_MAX, stall).await.unwrap_err();
            assert!(matches!(e, HomeserverError::TooLarge { .. }), "{e}");
        }
        // Stalled
        let started = std::time::Instant::now();
        let e = read_capped(get().await, OBJECT_MAX, stall).await.unwrap_err();
        assert!(matches!(e, HomeserverError::Timeout), "{e}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));

        let sent = server.join().unwrap();
        assert!(
            sent[1] < HUGE / 4,
            "read {} bytes of the one that said its size",
            sent[1]
        );
        assert!(
            sent[2] < HUGE / 4,
            "read {} bytes of the one that didn't",
            sent[2]
        );
    }
}

#[cfg(test)]
mod transient_tests {
    use super::HomeserverError;

    fn answered(code: u16) -> HomeserverError {
        HomeserverError::Pubky(pubky::Error::Request(pubky::errors::RequestError::Server {
            status: pubky::StatusCode::from_u16(code).unwrap(),
            message: String::new(),
        }))
    }

    #[test]
    fn only_5xx_answers_and_transport_failures_are_transient() {
        assert!(answered(502).is_transient() && answered(502).is_server_error());
        assert!(answered(503).is_server_error());
        assert!(!answered(404).is_transient() && !answered(404).is_server_error());
        assert!(!answered(408).is_transient());
        assert!(!HomeserverError::Timeout.is_transient());
        assert!(!HomeserverError::Transport("x".into()).is_server_error());
    }
}
