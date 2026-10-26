//! Sync the local music data with the user's homeserver (ADR 0014).
//!
//! Push: every row the library marks unsynced is written as its v1 object
//! (`/{root}/social/v1/posts/`, `/{root}/music/v1/posts/`, `/priv/social/v1/bookmarks/`), an edit
//! as a new version; tombstones delete every copy and are purged. Pull: private v1 objects the
//! node does not have, or has older, are imported, so a fresh node recovers its playlists. The
//! first sync on a node also reads the old `/priv/pubky.app/` folders and moves what only they
//! hold to v1, without deleting them.

use crate::playlists::{now_micros, store};
use crate::types::SyncReport;
use pm_library::Library;
use pm_pubky::homeserver::{HomeserverError, NodeHomeserver};
use pm_pubky::schemas::post::{TYPE_PLAYLIST, TYPE_REVIEW};
use pm_pubky::{v1, Favourite, MusicContent, MusicPost, Network, Visibility};
use pubky::PubkySession;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Homeserver(#[from] HomeserverError),
    #[error("library: {0}")]
    Library(#[from] pm_library::LibraryError),
    #[error("storing pulled post: {0}")]
    Store(String),
}

pub struct PubkySync {
    homeserver: NodeHomeserver,
    session: PubkySession,
    z32: String,
    /// Set when the homeserver refused a path for lack of access.
    refused: std::sync::atomic::AtomicBool,
    /// One pass at a time (a manual sync and the background carry-on never overlap).
    pass: tokio::sync::Mutex<()>,
}

/// At most this many writes and deletes per sync pass: a big change (thousands of favourites from
/// an import) goes out in bounded batches with a pause between them, and every object is marked
/// synced as it lands, so a sync cut short carries on where it stopped.
pub const WRITES_PER_PASS: usize = 200;
/// The pause between two passes of a big sync.
pub const PASS_PAUSE: std::time::Duration = std::time::Duration::from_secs(2);

/// After a sign-in: publish the node record, restore the settings and run the first sync in the
/// background, with progress in [`crate::state::NodeState::sync_activity`] and the durations in
/// the log.
pub fn first_sync_in_background(s: &Arc<crate::state::NodeState>, sync: Arc<PubkySync>) {
    let s = s.clone();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let step = |s: &crate::state::NodeState, words: &str| {
            let mut a = s.sync_activity.lock().expect("sync activity");
            a.running = true;
            a.step = Some(words.to_string());
            if a.started_at.is_none() || a.finished_at.is_some() {
                a.started_at = Some(pm_library::db::now_unix());
                a.finished_at = None;
                a.error = None;
            }
        };
        step(&s, "Telling your other devices where this node is");
        if let Err(e) = sync.publish_node_for(&s).await {
            tracing::warn!("publishing the node record failed: {e}");
        }
        step(&s, "Bringing your settings back");
        match sync.restore_settings(&s, true).await {
            Ok(Some(r)) => tracing::info!(?r, "settings restored from the homeserver"),
            Ok(None) => {}
            Err(e) => tracing::warn!("settings restore failed: {e}"),
        }
        step(&s, "Syncing your playlists, favourites and tags");
        let result = sync.sync(&s.library).await;
        let ms = started.elapsed().as_millis() as u64;
        {
            let mut a = s.sync_activity.lock().expect("sync activity");
            a.running = false;
            a.step = None;
            a.finished_at = Some(pm_library::db::now_unix());
            a.ms = Some(ms);
            match &result {
                Ok(r) => {
                    a.last = Some(r.clone());
                    a.error = None;
                }
                Err(e) => a.error = Some(e.to_string()),
            }
        }
        match result {
            Ok(r) => {
                tracing::info!(
                    ms,
                    pushed = r.pushed,
                    pulled = r.pulled,
                    pending = r.pending,
                    "pubky sign-in: first sync done"
                );
                sync.carry_on(&s, &r);
            }
            Err(e) => tracing::warn!(ms, "first sync failed: {e}"),
        }
        sync.backup_settings(&s).await;
        s.notify_library();
    });
}

/// Set once the node has moved its tags out of the old `pubky.app/tags/` folder.
const TAGS_MOVED: &str = "pubky.tags_moved";

/// What the owner reads when the session cannot reach every folder the node uses.
pub const RESIGN_NOTICE: &str = "This node needs permission for new folders on your homeserver. Sign in again with Pubky Ring to approve them.";

impl PubkySync {
    /// Restore a session from an exported secret and prepare to sync.
    pub async fn connect(network: Network, secret: &str) -> Result<Self, HomeserverError> {
        Self::connect_with(NodeHomeserver::new(network)?, secret).await
    }

    /// The same with the node's shared client ([`crate::state::NodeState::homeserver`]).
    pub async fn connect_with(homeserver: NodeHomeserver, secret: &str) -> Result<Self, HomeserverError> {
        let session = homeserver.restore_session(secret.trim()).await?;
        let z32 = session.public_key().to_string();
        Ok(Self {
            homeserver,
            session,
            z32,
            refused: Default::default(),
            pass: Default::default(),
        })
    }

    /// Wrap an existing session (tests, or a sign-in flow handled elsewhere).
    pub fn from_session(homeserver: NodeHomeserver, session: PubkySession) -> Self {
        let z32 = session.public_key().to_string();
        Self {
            homeserver,
            session,
            z32,
            refused: Default::default(),
            pass: Default::default(),
        }
    }

    /// The folders the node asks for at sign-in that this session was not granted.
    pub fn missing_capabilities(&self) -> Vec<String> {
        pm_pubky::homeserver::missing_capabilities(
            &pm_pubky::homeserver::session_capabilities(&self.session),
            pm_pubky::homeserver::LOGIN_CAPABILITIES,
        )
    }

    /// The session must be approved again: it lacks folders, or the homeserver refused one.
    pub fn needs_resign(&self) -> bool {
        self.refused.load(std::sync::atomic::Ordering::Relaxed) || !self.missing_capabilities().is_empty()
    }

    /// Remember a refusal (an access 403 from the homeserver).
    pub fn note_refusal(&self) {
        self.refused.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn session(&self) -> &PubkySession {
        &self.session
    }

    pub fn homeserver(&self) -> &NodeHomeserver {
        &self.homeserver
    }

    /// Tell the user's other devices where this node is (ADR 0008), starting remote access first
    /// when it is configured, so the record carries the node's iroh key (ADR 0013).
    pub async fn publish_node_for(&self, s: &crate::state::NodeState) -> Result<(), SyncError> {
        let iroh = s.ensure_remote().await;
        self.publish_node(
            &s.name,
            &s.public_url,
            s.token.as_deref(),
            &s.version,
            iroh.as_deref(),
        )
        .await
    }

    /// Tell the user's other devices where this node is (ADR 0008).
    pub async fn publish_node(
        &self,
        name: &str,
        api_url: &str,
        token: Option<&str>,
        version: &str,
        iroh: Option<&str>,
    ) -> Result<(), SyncError> {
        let record = pm_pubky::homeserver::NodeRecord {
            name: name.to_string(),
            api_url: api_url.to_string(),
            token: token.unwrap_or("").to_string(),
            version: version.to_string(),
            updated_at: now_micros(),
            iroh: iroh.unwrap_or("").to_string(),
        };
        Ok(self
            .homeserver
            .publish_node_record(&self.session, &record)
            .await?)
    }

    pub async fn list_private_files(&self) -> Result<Vec<pm_pubky::homeserver::RemoteFile>, SyncError> {
        Ok(self.homeserver.list_private_files(&self.session).await?)
    }

    /// The same, no faster than `bytes_per_sec`.
    pub async fn upload_private_paced(
        &self,
        path: &str,
        bytes: Vec<u8>,
        bytes_per_sec: Option<u64>,
    ) -> Result<(), SyncError> {
        Ok(self
            .homeserver
            .upload_private_paced(&self.session, path, bytes, bytes_per_sec)
            .await?)
    }

    pub async fn upload_private(&self, path: &str, bytes: Vec<u8>) -> Result<(), SyncError> {
        Ok(self.homeserver.upload_private(&self.session, path, bytes).await?)
    }

    pub async fn download_private(&self, path: &str) -> Result<Vec<u8>, SyncError> {
        Ok(self.homeserver.download_private(&self.session, path).await?)
    }

    pub async fn download_private_to(
        &self,
        path: &str,
        dest: &std::path::Path,
        bytes_per_sec: Option<u64>,
        progress: impl FnMut(u64) -> bool,
    ) -> Result<u64, SyncError> {
        Ok(self
            .homeserver
            .download_private_to(&self.session, path, dest, bytes_per_sec, progress)
            .await?)
    }

    pub async fn stat_private(&self, path: &str) -> Result<Option<u64>, SyncError> {
        Ok(self.homeserver.stat_private(&self.session, path).await?)
    }

    pub async fn delete_private(&self, path: &str) -> Result<(), SyncError> {
        Ok(self.homeserver.delete_private(&self.session, path).await?)
    }

    /// The signed-in user's raw z32 key.
    pub fn z32(&self) -> &str {
        &self.z32
    }

    /// One sync. A session the homeserver refuses (an old approval) ends with the plain
    /// [`RESIGN_NOTICE`] in the report and [`Self::needs_resign`] set, never a raw 403; the
    /// unsynced objects stay queued.
    pub async fn sync(&self, library: &Arc<Mutex<Library>>) -> Result<SyncReport, SyncError> {
        let _one_at_a_time = self.pass.lock().await;
        crate::tags::before_sync(&library.lock().expect("library poisoned"));
        match self.sync_once(library).await {
            Ok(mut report) => {
                if report
                    .errors
                    .iter()
                    .any(|e| e == RESIGN_NOTICE || is_forbidden(e))
                {
                    self.note_refusal();
                    report.errors.retain(|e| e != RESIGN_NOTICE && !is_forbidden(e));
                    report.errors.insert(0, RESIGN_NOTICE.to_string());
                }
                Ok(report)
            }
            Err(e) if is_forbidden(&e.to_string()) => {
                tracing::warn!("sync refused by the homeserver: {e}");
                self.note_refusal();
                Ok(SyncReport {
                    errors: vec![RESIGN_NOTICE.to_string()],
                    ..Default::default()
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Carry on with what a sync left pending, a pass at a time with [`PASS_PAUSE`] between,
    /// until nothing is left, a pass makes no progress, or another session replaced this one.
    pub fn carry_on(self: &Arc<Self>, s: &Arc<crate::state::NodeState>, report: &SyncReport) {
        if report.pending == 0 {
            return;
        }
        tracing::info!(pending = report.pending, "syncing the rest in batches");
        let (me, s) = (self.clone(), s.clone());
        let mut left = report.pending;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PASS_PAUSE).await;
                if !s.sync_handle().is_some_and(|h| Arc::ptr_eq(&h, &me)) {
                    break;
                }
                match me.sync(&s.library).await {
                    Ok(r) => {
                        if r.pulled > 0 {
                            s.notify_library();
                        }
                        if r.pending == 0 || r.pending >= left {
                            tracing::info!(pending = r.pending, "batched sync done");
                            break;
                        }
                        left = r.pending;
                    }
                    Err(e) => {
                        tracing::warn!("batched sync stopped: {e}");
                        break;
                    }
                }
            }
        });
    }

    async fn sync_once(&self, library: &Arc<Mutex<Library>>) -> Result<SyncReport, SyncError> {
        let mut report = SyncReport::default();
        let owner = v1::PubkyId::try_from(raw_key(&self.z32))
            .map_err(|e| SyncError::Store(format!("signed-in key: {e}")))?;
        let migrated = library
            .lock()
            .expect("library poisoned")
            .get_setting(V1_MIGRATED)?
            .is_some_and(|v| !v.is_empty());

        // Tags: public PubkyAppTags under the node's namespace (Nexus indexes
        // tags on external links only there), pushed and pulled the same way.
        // Tags written under pubky.app/tags/ before are moved first.
        // Once per node: the old folder is read in full (it holds other apps' tags too), so a
        // sync that does it every time costs hundreds of requests
        let mut tags_moved = library
            .lock()
            .expect("library poisoned")
            .get_setting(TAGS_MOVED)?
            .is_some();
        if !tags_moved {
            match self.homeserver.migrate_music_tags(&self.session).await {
                Ok((moved, kept)) => {
                    if moved > 0 {
                        tracing::info!(
                            moved,
                            kept,
                            "tags moved to /pub/pubky.music/tags/ so Nexus indexes them"
                        );
                        report.pushed += moved;
                    }
                    library
                        .lock()
                        .expect("library poisoned")
                        .set_setting(TAGS_MOVED, &now_micros().to_string())?;
                    tags_moved = true;
                }
                Err(e) => report.errors.push(tag_write_hint(format!(
                    "moving tags to /pub/pubky.music/tags/: {e}"
                ))),
            }
        }
        let pending = library.lock().expect("library poisoned").unsynced_tags()?;
        let total = pending.len();
        for (done, row) in pending.into_iter().enumerate() {
            if spent(&report) {
                report.pending += total - done;
                break;
            }
            let path = pm_pubky::schemas::music_tag_path(&row.id);
            if row.deleted {
                // Also the legacy path, for a tag written there and never moved.
                let _ = self
                    .homeserver
                    .delete(
                        &self.session,
                        &format!("{}{}", pm_pubky::schemas::LEGACY_TAG_DIR, row.id),
                    )
                    .await;
                match self.homeserver.delete(&self.session, &path).await {
                    Ok(()) => report.deleted += 1,
                    Err(HomeserverError::Pubky(e)) if e.to_string().contains("404") => report.deleted += 1,
                    Err(e) => {
                        report.errors.push(format!("delete {path}: {e}"));
                        continue;
                    }
                }
            } else {
                let tag = pubky_app_specs::PubkyAppTag {
                    uri: row.uri.clone(),
                    label: row.label.clone(),
                    created_at: row.created_at,
                };
                if let Err(e) = self.homeserver.write_object(&self.session, &path, &tag).await {
                    report.errors.push(tag_write_hint(format!("write {path}: {e}")));
                    continue;
                }
                report.pushed += 1;
            }
            library
                .lock()
                .expect("library poisoned")
                .mark_tag_synced(&row.id, now_micros())?;
        }
        let known_tags = library.lock().expect("library poisoned").tag_ids()?;
        match self
            .homeserver
            .read_music_tags_in(&self.z32, &known_tags, !tags_moved)
            .await
        {
            Ok(remote_tags) => {
                let lib = library.lock().expect("library poisoned");
                for (path, t) in remote_tags {
                    let id = path.rsplit('/').next().unwrap_or("").to_string();
                    if lib.tag_by_id(&id)?.is_none() {
                        lib.add_tag(&id, &t.uri, &t.label, t.created_at)?;
                        lib.mark_tag_synced(&id, now_micros())?;
                        report.pulled += 1;
                    }
                }
            }
            Err(e) => report.errors.push(format!("reading tags: {e}")),
        }

        // Before the move to v1 (ADR 0014): what only the old folders have comes in first, so a
        // fresh node recovers everything. The old files are never deleted by the migration.
        if !migrated {
            self.pull_legacy(library, &mut report).await?;
        }

        // What the homeserver has in v1.
        let mut heads = Heads::default();
        for root in [v1::Root::Priv, v1::Root::Pub] {
            for ns in [v1::SOCIAL_NAMESPACE, pubky_music_specs::MUSIC_NAMESPACE] {
                let visibility = if root == v1::Root::Pub {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                let session = (visibility == Visibility::Private).then_some(&self.session);
                let found = self.homeserver.v1_heads(&self.z32, root, ns, session).await?;
                heads.0.insert((is_pub(root), ns), found);
            }
        }
        // Favourites the node has are listed, not downloaded again
        let known_favs: std::collections::HashMap<String, (pm_model::MusicUri, i64)> = library
            .lock()
            .expect("library poisoned")
            .list_favourites()?
            .into_iter()
            .filter_map(|f| {
                let uri = pm_model::MusicUri::parse(&f.uri).ok()?;
                let path = v1::favourite_bookmark_path(&uri).ok()?;
                Some((path.rsplit('/').next()?.to_string(), (uri, f.created_at)))
            })
            .collect();
        let remote_favs = self
            .homeserver
            .read_v1_favourites_except(&self.session, &known_favs)
            .await?;

        // The migration: queue every object the homeserver has only in the old format.
        if !migrated {
            let lib = library.lock().expect("library poisoned");
            let mut queued = 0;
            for row in lib.list_music_posts(None)? {
                if !heads.has(&row.id) {
                    lib.mark_music_post_unsynced(&row.id)?;
                    queued += 1;
                }
            }
            let on_v1: HashSet<String> = remote_favs.iter().map(|(_, u, _)| u.to_url()).collect();
            for f in lib.list_favourites()? {
                if !on_v1.contains(&f.uri) {
                    lib.mark_favourite_unsynced(&f.id)?;
                    queued += 1;
                }
            }
            if queued > 0 {
                tracing::info!(queued, "moving music data to social/v1 and music/v1");
            }
        }

        // Playlists the homeserver has only in the old format (the long ones, kept back before
        // collections lost their size cap): write them as v1 collections now. Old files stay.
        {
            let lib = library.lock().expect("library poisoned");
            for row in lib.list_music_posts(Some(pm_pubky::schemas::post::TYPE_PLAYLIST))? {
                if row.synced_at.is_some() && !heads.has(&row.id) {
                    lib.mark_music_post_unsynced(&row.id)?;
                }
            }
        }
        self.push_posts(library, &owner, &mut heads, &mut report).await?;
        let removed_favs = self.push_favourites(library, &mut report).await?;
        self.pull_v1(library, &heads, &mut report).await?;

        // Favourites the homeserver has and this node does not.
        {
            let lib = library.lock().expect("library poisoned");
            for (_, uri, created_at) in remote_favs {
                let url = uri.to_url();
                if removed_favs.contains(&url) || lib.favourite_by_uri(&url)?.is_some() {
                    continue;
                }
                let id = Favourite::id_for(&uri);
                lib.add_favourite(&id, &url, created_at)?;
                lib.mark_favourite_synced(&id, now_micros())?;
                report.pulled += 1;
            }
            if !migrated && report.errors.is_empty() && report.pending == 0 {
                lib.set_setting(V1_MIGRATED, &now_micros().to_string())?;
                tracing::info!("music data is on social/v1 and music/v1");
            }
        }
        Ok(report)
    }

    /// The old folders (`/priv/pubky.app/posts/`, `/priv/pubky.app/bookmarks/`), read until the
    /// migration has run on this node.
    async fn pull_legacy(
        &self,
        library: &Arc<Mutex<Library>>,
        report: &mut SyncReport,
    ) -> Result<(), SyncError> {
        let remote_favs = self
            .homeserver
            .read_favourites(&self.z32, Visibility::Private, Some(&self.session))
            .await?;
        let remote = self
            .homeserver
            .read_music_posts(&self.z32, Visibility::Private, Some(&self.session))
            .await?;
        let lib = library.lock().expect("library poisoned");
        for (_, f) in remote_favs {
            let url = f.uri.to_url();
            if lib.favourite_by_uri(&url)?.is_none() {
                lib.add_favourite(&f.id, &url, f.created_at)?;
                lib.mark_favourite_synced(&f.id, now_micros())?;
                report.pulled += 1;
            }
        }
        for (path, post) in remote {
            let id = path.rsplit('/').next().unwrap_or("").to_string();
            if !matches!(post.content.type_name(), t if t == TYPE_PLAYLIST || t == TYPE_REVIEW) {
                continue;
            }
            let remote_updated = match &post.content {
                MusicContent::Playlist(p) => p.updated_at,
                other => other.created_micros(),
            };
            let take = match lib.get_music_post(&id)? {
                None => true,
                Some(row) => remote_updated > row.updated_at,
            };
            if take {
                store(&lib, &post, &id, Visibility::Private).map_err(|e| SyncError::Store(e.message))?;
                lib.mark_music_post_synced(&id, now_micros())?;
                report.pulled += 1;
            }
        }
        Ok(())
    }

    /// Push every unsynced post as its v1 object; tombstones delete every copy.
    async fn push_posts(
        &self,
        library: &Arc<Mutex<Library>>,
        owner: &v1::PubkyId,
        heads: &mut Heads,
        report: &mut SyncReport,
    ) -> Result<(), SyncError> {
        let pending = library.lock().expect("library poisoned").unsynced_music_posts()?;
        let total = pending.len();
        for (done, row) in pending.into_iter().enumerate() {
            if spent(report) {
                report.pending += total - done;
                break;
            }
            let public = row.visibility == "public";
            // Every version is written privately (the draft); a public playlist is also published:
            // the same version copied under /pub/ (pubky-social-specs plan_publish)
            let root = v1::Root::Priv;
            if row.deleted {
                // The old copies first, then every version, oldest first, public before private
                let mut paths = vec![
                    MusicPost::path(&row.id, Visibility::Public),
                    MusicPost::path(&row.id, Visibility::Private),
                ];
                paths.extend(heads.take_all(&row.id));
                let mut failed = false;
                for path in &paths {
                    if let Err(e) = self.delete_quietly(path).await {
                        report.errors.push(format!("delete {path}: {e}"));
                        failed = true;
                    }
                }
                if failed {
                    continue;
                }
                report.deleted += 1;
            } else {
                let content = match serde_json::from_str::<pubky_app_specs::PubkyAppPost>(&row.json)
                    .map_err(|e| e.to_string())
                    .and_then(|p| MusicPost::from_post(p, Some(&row.id)).map_err(|e| e.to_string()))
                {
                    Ok(Some(post)) => post.content,
                    Ok(None) => continue,
                    Err(e) => {
                        report.errors.push(format!("{}: stored json: {e}", row.id));
                        continue;
                    }
                };
                match v1::to_v1(&content) {
                    Ok(post) => {
                        let ns = post.namespace();
                        let head = heads.edit_id(root, ns, &row.id);
                        let path = match post.version_path(&row.id, head.as_deref(), root, owner) {
                            Ok(p) => p,
                            Err(e) => {
                                report.errors.push(format!("{}: {e}", row.id));
                                continue;
                            }
                        };
                        if let Err(e) = self
                            .homeserver
                            .write_object(&self.session, &path, &post.to_json())
                            .await
                        {
                            let msg = format!("write {path}: {e}");
                            let stop = is_forbidden(&msg);
                            report.errors.push(namespace_hint(msg));
                            if stop {
                                // The session cannot write here; every other write would fail too
                                break;
                            }
                            continue;
                        }
                        // The same id in the other namespace (a review that gained or lost
                        // its rating) would read back twice
                        for stale in heads.take_other(root, ns, &row.id) {
                            let _ = self.delete_quietly(&stale).await;
                        }
                        heads.record(root, ns, &row.id, &path);
                        if let Err(e) = self
                            .publish_or_unpublish(&post, public, &row.id, &path, ns, owner, heads)
                            .await
                        {
                            let stop = is_forbidden(&e);
                            report.errors.push(namespace_hint(e));
                            if stop {
                                break;
                            }
                            continue;
                        }
                    }
                    Err(e) => {
                        report.errors.push(format!("{}: {e}", row.id));
                        continue;
                    }
                }
                report.pushed += 1;
            }
            library
                .lock()
                .expect("library poisoned")
                .mark_music_post_synced(&row.id, now_micros())?;
        }
        Ok(())
    }

    /// Push favourites as v1 bookmarks. Returns the URLs removed, so the pull does not bring
    /// them back from the listing taken before.
    async fn push_favourites(
        &self,
        library: &Arc<Mutex<Library>>,
        report: &mut SyncReport,
    ) -> Result<HashSet<String>, SyncError> {
        let mut removed = HashSet::new();
        let pending = library.lock().expect("library poisoned").unsynced_favourites()?;
        let total = pending.len();
        for (done, row) in pending.into_iter().enumerate() {
            if spent(report) {
                report.pending += total - done;
                break;
            }
            let Ok(uri) = pm_model::MusicUri::parse(&row.uri) else {
                report
                    .errors
                    .push(format!("{}: not a canonical music URL", row.uri));
                continue;
            };
            if row.deleted {
                let legacy = Favourite::path(&row.id, Visibility::Private);
                let mut failed = false;
                let mut paths = vec![legacy];
                match v1::favourite_bookmark_path(&uri) {
                    Ok(p) => paths.push(p),
                    Err(e) => report.errors.push(format!("{}: {e}", row.uri)),
                }
                for path in &paths {
                    if let Err(e) = self.delete_quietly(path).await {
                        report.errors.push(format!("delete {path}: {e}"));
                        failed = true;
                    }
                }
                if failed {
                    continue;
                }
                removed.insert(row.uri.clone());
                report.deleted += 1;
            } else {
                let (bookmark, path) = match v1::favourite_bookmark(&uri, row.created_at) {
                    Ok(b) => b,
                    Err(e) => {
                        report.errors.push(format!("{}: {e}", row.uri));
                        continue;
                    }
                };
                if let Err(e) = self
                    .homeserver
                    .write_object(&self.session, &path, &bookmark)
                    .await
                {
                    let msg = format!("write {path}: {e}");
                    let stop = is_forbidden(&msg);
                    report.errors.push(namespace_hint(msg));
                    if stop {
                        break;
                    }
                    continue;
                }
                report.pushed += 1;
            }
            library
                .lock()
                .expect("library poisoned")
                .mark_favourite_synced(&row.id, now_micros())?;
        }
        Ok(removed)
    }

    /// Bring in private v1 playlists and reviews that are new or newer than the node's copy.
    /// Listens stay on the homeserver (the node keeps its own history).
    async fn pull_v1(
        &self,
        library: &Arc<Mutex<Library>>,
        heads: &Heads,
        report: &mut SyncReport,
    ) -> Result<(), SyncError> {
        for ns in [v1::SOCIAL_NAMESPACE, pubky_music_specs::MUSIC_NAMESPACE] {
            let Some(found) = heads.0.get(&(false, ns)) else {
                continue;
            };
            // What is new or newer than the node's copy, then fetched 8 at a time
            let wanted: Vec<(String, v1::Head)> = {
                let lib = library.lock().expect("library poisoned");
                let mut w = Vec::new();
                for (id, head) in found {
                    let take = match lib.get_music_post(id)? {
                        None => true,
                        Some(row) => head.edited_micros() > row.updated_at.max(row.synced_at.unwrap_or(0)),
                    };
                    if take {
                        w.push((id.clone(), head.clone()));
                    }
                }
                w
            };
            use futures_util::StreamExt;
            let this = self;
            let fetched: Vec<(String, v1::Head, Result<Vec<u8>, HomeserverError>)> =
                futures_util::stream::iter(wanted)
                    .map(move |(id, head)| async move {
                        let r = this
                            .homeserver
                            .get_bytes(&this.z32, &head.path, Visibility::Private, Some(&this.session))
                            .await;
                        (id, head, r)
                    })
                    .buffered(pm_pubky::homeserver::FETCH_AT_ONCE)
                    .collect()
                    .await;
            for (id, head, bytes) in fetched {
                let bytes = match bytes {
                    Ok(b) => b,
                    Err(e) => {
                        report.errors.push(format!("read {}: {e}", head.path));
                        continue;
                    }
                };
                let content = if ns == v1::SOCIAL_NAMESPACE {
                    serde_json::from_slice::<pubky_social_specs::PubkySocialPost>(&bytes)
                        .ok()
                        .and_then(|p| v1::from_social(&p, &id, &head.edit_id))
                } else {
                    serde_json::from_slice::<pubky_music_specs::MusicPost>(&bytes)
                        .ok()
                        .and_then(|p| v1::from_music(&p, &id))
                };
                let Some(content) = content else {
                    continue;
                };
                let post = MusicPost::new(content);
                if let Err(e) = post.validate(Some(&id)) {
                    tracing::warn!(path = %head.path, error = %e, "skipping a v1 post the node cannot hold");
                    continue;
                }
                let lib = library.lock().expect("library poisoned");
                store(&lib, &post, &id, Visibility::Private).map_err(|e| SyncError::Store(e.message))?;
                lib.mark_music_post_synced(&id, now_micros())?;
                report.pulled += 1;
            }
        }
        Ok(())
    }

    /// Delete, counting "already gone" as done.
    async fn delete_quietly(&self, path: &str) -> Result<(), HomeserverError> {
        match self.homeserver.delete(&self.session, path).await {
            Err(HomeserverError::Pubky(e)) if e.to_string().contains("404") => Ok(()),
            other => other,
        }
    }
}

/// Set once this node has moved its music data to v1 (ADR 0014); until then every sync reads
/// the old folders first and queues what v1 lacks.
const V1_MIGRATED: &str = "pubky.v1_migrated";

/// The bare z32 key, without a `pubky` prefix.
/// This pass has written all it may.
fn spent(report: &SyncReport) -> bool {
    report.pushed + report.deleted >= WRITES_PER_PASS
}

fn raw_key(z32: &str) -> &str {
    z32.strip_prefix("pubky").filter(|k| k.len() == 52).unwrap_or(z32)
}

fn is_pub(root: v1::Root) -> bool {
    root == v1::Root::Pub
}

impl PubkySync {
    /// Publish (`public`): copy the version just written privately to its public path.
    /// Unpublish (not `public`): remove every public version. Only social posts (playlists)
    /// are published; the others stay private.
    #[allow(clippy::too_many_arguments)]
    async fn publish_or_unpublish(
        &self,
        post: &v1::V1Post,
        public: bool,
        id: &str,
        private_path: &str,
        ns: &'static str,
        owner: &v1::PubkyId,
        heads: &mut Heads,
    ) -> Result<(), String> {
        let v1::V1Post::Social(social) = post else {
            return Ok(());
        };
        if !public {
            for path in heads.take_in(v1::Root::Pub, ns, id) {
                self.delete_quietly(&path)
                    .await
                    .map_err(|e| format!("unpublish {path}: {e}"))?;
            }
            return Ok(());
        }
        let Some((_, edit_id)) = v1::parse_version(private_path) else {
            return Err(format!("{id}: not a version path: {private_path}"));
        };
        let (dest, body) =
            v1::publish_plan(social, id, &edit_id, owner).map_err(|e| format!("publish {id}: {e}"))?;
        self.homeserver
            .write_object(&self.session, &dest, &body)
            .await
            .map_err(|e| format!("write {dest}: {e}"))?;
        heads.record(v1::Root::Pub, ns, id, &dest);
        Ok(())
    }
}

/// The newest version of every v1 post, per root (public or not) and namespace.
#[derive(Default)]
struct Heads(BTreeMap<(bool, &'static str), BTreeMap<String, v1::Head>>);

impl Heads {
    fn has(&self, id: &str) -> bool {
        self.0.values().any(|m| m.contains_key(id))
    }

    fn edit_id(&self, root: v1::Root, ns: &'static str, id: &str) -> Option<String> {
        self.0
            .get(&(is_pub(root), ns))?
            .get(id)
            .map(|h| h.edit_id.clone())
    }

    /// Every version of `id` everywhere, removed from the listing: oldest first, public first.
    fn take_all(&mut self, id: &str) -> Vec<String> {
        let mut out = Vec::new();
        for public in [true, false] {
            for ((p, _), m) in self.0.iter_mut() {
                if *p == public {
                    if let Some(h) = m.remove(id) {
                        out.extend(h.versions);
                    }
                }
            }
        }
        out
    }

    /// The versions of `id` in `root` and `ns`, removed from the listing.
    fn take_in(&mut self, root: v1::Root, ns: &'static str, id: &str) -> Vec<String> {
        self.0
            .get_mut(&(is_pub(root), ns))
            .and_then(|m| m.remove(id))
            .map(|h| h.versions)
            .unwrap_or_default()
    }

    /// The versions of `id` in the other namespaces of `root`, removed from the listing.
    fn take_other(&mut self, root: v1::Root, ns: &'static str, id: &str) -> Vec<String> {
        let mut out = Vec::new();
        for ((p, n), m) in self.0.iter_mut() {
            if *p == is_pub(root) && *n != ns {
                if let Some(h) = m.remove(id) {
                    out.extend(h.versions);
                }
            }
        }
        out
    }

    fn record(&mut self, root: v1::Root, ns: &'static str, id: &str, path: &str) {
        let Some((_, edit_id)) = v1::parse_version(path) else {
            return;
        };
        let head = self
            .0
            .entry((is_pub(root), ns))
            .or_default()
            .entry(id.to_string())
            .or_insert_with(|| v1::Head {
                edit_id: String::new(),
                path: String::new(),
                versions: Vec::new(),
            });
        head.edit_id = edit_id;
        head.path = path.to_string();
        head.versions.push(path.to_string());
    }
}

/// Whether a homeserver error is an access refusal (an old approval).
pub(crate) fn is_refusal(msg: &str) -> bool {
    is_forbidden(msg)
}

fn is_forbidden(msg: &str) -> bool {
    pm_pubky::homeserver::is_capability_error(msg) || msg.to_lowercase().contains("capabilit")
}

/// A session approved before the node used its current folders: the plain notice, never the
/// homeserver's raw 403.
fn namespace_hint(msg: String) -> String {
    if is_forbidden(&msg) {
        tracing::warn!("homeserver refused a path: {msg}");
        RESIGN_NOTICE.to_string()
    } else {
        msg
    }
}

impl std::fmt::Debug for PubkySync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PubkySync").field("z32", &self.z32).finish()
    }
}

/// Settings keys that belong to the owner, not to a machine: streaming
/// sessions, renderers added by hand, the Subsonic jukebox zone, DSP per
/// output. Caches (`tidal.collection`, `tidal.version.*`, `artist.info.*`,
/// `mb.rg.*`, `pinakes.version.*`) are not backed up.
const BACKED_UP_SETTING_PREFIXES: [&str; 9] = [
    "tidal.user",
    "tidal.playback_limit",
    "outputs.upnp_addresses",
    "outputs.endpoint_addresses",
    "subsonic.jukebox_zone",
    "dsp/",
    "library.",
    "home.layout",
    "smart.",
];

/// What `/priv/pubky.music/settings.json` holds.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct SettingsBackup {
    pub version: u32,
    pub saved_at: i64,
    pub node: String,
    #[serde(default)]
    pub contact: String,
    #[serde(default)]
    pub folders: Vec<String>,
    #[serde(default)]
    pub settings: std::collections::BTreeMap<String, String>,
}

impl PubkySync {
    /// The person's own preferences on their homeserver (`prefs.json`), when there are any.
    pub async fn read_prefs(&self) -> Result<Option<crate::prefs::Prefs>, SyncError> {
        let Some(v) = self
            .homeserver
            .read_prefs(&self.session)
            .await
            .map_err(|e| SyncError::Store(e.to_string()))?
        else {
            return Ok(None);
        };
        Ok(serde_json::from_value(v).ok())
    }

    /// Save the person's own preferences on their homeserver.
    pub async fn write_prefs(&self, p: &crate::prefs::Prefs) -> Result<(), SyncError> {
        let v = serde_json::to_value(p).map_err(|e| SyncError::Store(e.to_string()))?;
        self.homeserver
            .write_prefs(&self.session, &v)
            .await
            .map_err(|e| SyncError::Store(e.to_string()))
    }

    /// Snapshot the owner's settings into the private folder (after every
    /// sync and every change the API makes). Best effort; errors are logged.
    pub async fn backup_settings(&self, s: &crate::state::NodeState) {
        let backup = {
            let lib = s.library.lock().expect("library poisoned");
            let mut settings = std::collections::BTreeMap::new();
            for prefix in BACKED_UP_SETTING_PREFIXES {
                if let Ok(rows) = lib.settings_with_prefix(prefix) {
                    settings.extend(rows);
                }
            }
            SettingsBackup {
                version: 1,
                saved_at: pm_library::db::now_unix(),
                node: s.name.clone(),
                contact: s.contact(),
                folders: lib
                    .list_folders()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(p, _)| p.to_string_lossy().into_owned())
                    .collect(),
                settings,
            }
        };
        let v = serde_json::to_value(&backup).expect("serializes");
        if let Err(e) = self.homeserver.write_node_settings(&self.session, &v).await {
            tracing::warn!("settings backup failed: {e}");
        }
    }

    /// Bring the owner's settings onto this node from the private folder.
    /// `only_if_fresh`: skip when the node already has folders or a contact
    /// (a running node keeps what it has). Returns what was applied.
    pub async fn restore_settings(
        &self,
        s: &Arc<crate::state::NodeState>,
        only_if_fresh: bool,
    ) -> Result<Option<SettingsRestored>, SyncError> {
        let Some(v) = self.homeserver.read_node_settings(&self.session).await? else {
            return Ok(None);
        };
        let backup: SettingsBackup =
            serde_json::from_value(v).map_err(|e| SyncError::Store(format!("settings backup: {e}")))?;
        let fresh = {
            let lib = s.library.lock().expect("library poisoned");
            lib.list_folders().map(|f| f.is_empty()).unwrap_or(true) && s.contact().trim().is_empty()
        };
        if only_if_fresh && !fresh {
            return Ok(None);
        }
        let mut out = SettingsRestored {
            from_node: backup.node.clone(),
            saved_at: backup.saved_at,
            ..Default::default()
        };
        if s.contact().trim().is_empty()
            && !backup.contact.trim().is_empty()
            && s.set_contact(&backup.contact).is_ok()
        {
            out.contact = true;
        }
        let mut new_folders = Vec::new();
        {
            let lib = s.library.lock().expect("library poisoned");
            let have: std::collections::HashSet<String> = lib
                .list_folders()
                .unwrap_or_default()
                .into_iter()
                .map(|(p, _)| p.to_string_lossy().into_owned())
                .collect();
            for f in &backup.folders {
                let p = std::path::PathBuf::from(f);
                if have.contains(f) {
                    continue;
                }
                if p.is_dir() {
                    if lib.add_folder(&p).is_ok() {
                        new_folders.push(p);
                    }
                } else {
                    out.folders_missing.push(f.clone());
                }
            }
            for (k, val) in &backup.settings {
                let keep = lib
                    .get_setting(k)
                    .ok()
                    .flatten()
                    .is_some_and(|cur| !cur.is_empty());
                if !keep && lib.set_setting(k, val).is_ok() {
                    out.settings += 1;
                }
            }
        }
        for p in &new_folders {
            s.watch_more(p);
        }
        if !new_folders.is_empty() {
            out.folders = new_folders
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            let _ = s.start_scan(new_folders);
        }
        Ok(Some(out))
    }
}

/// Report of a settings restore.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct SettingsRestored {
    pub from_node: String,
    pub saved_at: i64,
    pub contact: bool,
    pub folders: Vec<String>,
    pub folders_missing: Vec<String>,
    pub settings: u32,
}

/// A session approved before the tag namespace moved cannot write
/// `/pub/pubky.music/`: say what to do about it.
fn tag_write_hint(msg: String) -> String {
    if msg.contains("403")
        || msg.to_lowercase().contains("forbidden")
        || msg.to_lowercase().contains("capabilit")
    {
        format!("{msg} (sign the node in with Pubky Ring again: tags now go to /pub/pubky.music/, which this session may not write)")
    } else {
        msg
    }
}
