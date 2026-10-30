//! Local store of the user's music posts (ADR 0006): the node keeps the
//! working copy and syncs the private homeserver folder when a session
//! exists. Rows hold the post JSON verbatim; the schema lives in `pm-pubky`.

use crate::db::Library;
use crate::error::Result;
use rusqlite::{params, OptionalExtension, Row};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicPostRow {
    /// Timestamp id, also the last path segment.
    pub id: String,
    /// Envelope type, e.g. `pubky.music/playlist`.
    pub kind: String,
    /// `private` or `public`.
    pub visibility: String,
    /// The `PubkyAppPost` JSON as written to the homeserver.
    pub json: String,
    /// Microseconds, from the envelope.
    pub created_at: i64,
    pub updated_at: i64,
    /// Last successful push (microseconds), `None` when never pushed.
    pub synced_at: Option<i64>,
    pub deleted: bool,
}

fn row(r: &Row<'_>) -> rusqlite::Result<MusicPostRow> {
    Ok(MusicPostRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        visibility: r.get(2)?,
        json: r.get(3)?,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
        synced_at: r.get(6)?,
        deleted: r.get::<_, i64>(7)? != 0,
    })
}

const COLS: &str = "id, kind, visibility, json, created_at, updated_at, synced_at, deleted";

impl Library {
    /// Insert or replace a post. Clears `synced_at` unless the row says otherwise.
    pub fn upsert_music_post(&self, p: &MusicPostRow) -> Result<()> {
        self.lock().execute(
            "INSERT INTO music_posts (id, kind, visibility, json, created_at, updated_at, synced_at, deleted)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, visibility = excluded.visibility,
               json = excluded.json, created_at = excluded.created_at, updated_at = excluded.updated_at,
               synced_at = excluded.synced_at, deleted = excluded.deleted",
            params![
                p.id,
                p.kind,
                p.visibility,
                p.json,
                p.created_at,
                p.updated_at,
                p.synced_at,
                p.deleted as i64
            ],
        )?;
        Ok(())
    }

    pub fn get_music_post(&self, id: &str) -> Result<Option<MusicPostRow>> {
        Ok(self
            .lock()
            .query_row(
                &format!("SELECT {COLS} FROM music_posts WHERE id = ?1 AND deleted = 0"),
                [id],
                row,
            )
            .optional()?)
    }

    /// Live posts of one kind (or all), newest edit first.
    pub fn list_music_posts(&self, kind: Option<&str>) -> Result<Vec<MusicPostRow>> {
        let conn = self.lock();
        let mut out = Vec::new();
        match kind {
            Some(k) => {
                let mut st = conn.prepare(&format!(
                    "SELECT {COLS} FROM music_posts WHERE kind = ?1 AND deleted = 0 ORDER BY updated_at DESC"
                ))?;
                for r in st.query_map([k], row)? {
                    out.push(r?);
                }
            }
            None => {
                let mut st = conn.prepare(&format!(
                    "SELECT {COLS} FROM music_posts WHERE deleted = 0 ORDER BY updated_at DESC"
                ))?;
                for r in st.query_map([], row)? {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    /// Tombstone a post (it stays until the remote delete is confirmed).
    pub fn delete_music_post(&self, id: &str, now_micros: i64) -> Result<bool> {
        let n = self.lock().execute(
            "UPDATE music_posts SET deleted = 1, updated_at = ?2 WHERE id = ?1 AND deleted = 0",
            params![id, now_micros],
        )?;
        Ok(n > 0)
    }

    /// Posts that need a push: never synced, edited since, or tombstoned.
    pub fn unsynced_music_posts(&self) -> Result<Vec<MusicPostRow>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {COLS} FROM music_posts WHERE synced_at IS NULL OR updated_at > synced_at OR deleted = 1
             ORDER BY updated_at"
        ))?;
        let mut out = Vec::new();
        for r in st.query_map([], row)? {
            out.push(r?);
        }
        Ok(out)
    }

    /// Queue a live post for another push (the v1 migration re-writes posts the homeserver has
    /// only in the old format).
    pub fn mark_music_post_unsynced(&self, id: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE music_posts SET synced_at = NULL WHERE id = ?1 AND deleted = 0",
            [id],
        )?;
        Ok(())
    }

    /// Record a successful push of `pushed` (as it was read for the push), only if the post is
    /// still that: a change made while the push ran (a move to the other folder, an edit, a
    /// delete) stays unsynced for the next pass. A pushed tombstone is purged. `true` when marked.
    pub fn mark_music_post_synced_if_unchanged(&self, pushed: &MusicPostRow, at_micros: i64) -> Result<bool> {
        let conn = self.lock();
        let n = if pushed.deleted {
            conn.execute(
                "DELETE FROM music_posts WHERE id = ?1 AND deleted = 1 AND updated_at = ?2",
                params![pushed.id, pushed.updated_at],
            )?
        } else {
            conn.execute(
                "UPDATE music_posts SET synced_at = ?2
                 WHERE id = ?1 AND deleted = 0 AND updated_at = ?3 AND visibility = ?4 AND json = ?5",
                params![
                    pushed.id,
                    at_micros,
                    pushed.updated_at,
                    pushed.visibility,
                    pushed.json
                ],
            )?
        };
        Ok(n > 0)
    }

    /// Record a successful push; a tombstone is purged.
    pub fn mark_music_post_synced(&self, id: &str, at_micros: i64) -> Result<()> {
        let conn = self.lock();
        let purged = conn.execute("DELETE FROM music_posts WHERE id = ?1 AND deleted = 1", [id])?;
        if purged == 0 {
            conn.execute(
                "UPDATE music_posts SET synced_at = ?2 WHERE id = ?1",
                params![id, at_micros],
            )?;
        }
        Ok(())
    }
}

impl Library {
    /// Live posts of one kind whose `updated_at` is before `before` (all when `None`), newest
    /// first, at most `limit`: a page of a history (a listen's `updated_at` is its play time).
    pub fn music_posts_page(&self, kind: &str, before: Option<i64>, limit: u32) -> Result<Vec<MusicPostRow>> {
        let conn = self.lock();
        let mut st = conn.prepare_cached(&format!(
            "SELECT {COLS} FROM music_posts WHERE kind = ?1 AND deleted = 0 AND updated_at < ?2
             ORDER BY updated_at DESC, id DESC LIMIT ?3"
        ))?;
        let rows = st
            .query_map(params![kind, before.unwrap_or(i64::MAX), limit as i64], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Live posts of one kind with `updated_at` in `from..=to`.
    pub fn music_posts_between(&self, kind: &str, from: i64, to: i64) -> Result<Vec<MusicPostRow>> {
        let conn = self.lock();
        let mut st = conn.prepare_cached(&format!(
            "SELECT {COLS} FROM music_posts WHERE kind = ?1 AND deleted = 0 AND updated_at BETWEEN ?2 AND ?3
             ORDER BY updated_at DESC"
        ))?;
        let rows = st
            .query_map(params![kind, from, to], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// How many live posts of one kind each visibility has: `(private, public)`.
    pub fn music_post_counts(&self, kind: &str) -> Result<(u64, u64)> {
        let conn = self.lock();
        let n = |vis: &str| -> Result<u64> {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM music_posts WHERE kind = ?1 AND deleted = 0 AND visibility = ?2",
                params![kind, vis],
                |r| r.get::<_, i64>(0),
            )? as u64)
        };
        Ok((n("private")?, n("public")?))
    }

    /// Give every live post of one kind this visibility; the ones that change are queued for the
    /// next push (which moves them). Returns how many changed.
    pub fn set_music_posts_visibility(&self, kind: &str, visibility: &str) -> Result<usize> {
        Ok(self.lock().execute(
            "UPDATE music_posts SET visibility = ?2, synced_at = NULL
             WHERE kind = ?1 AND deleted = 0 AND visibility != ?2",
            params![kind, visibility],
        )?)
    }

    /// Tombstone the live posts of one kind **written** at or before `max_id` (a timestamp id:
    /// those sort by time), e.g. the listens a history delete on another node covers. Returns
    /// how many.
    pub fn delete_music_posts_of_kind_up_to(
        &self,
        kind: &str,
        max_id: &str,
        now_micros: i64,
    ) -> Result<usize> {
        Ok(self.lock().execute(
            "UPDATE music_posts SET deleted = 1, updated_at = ?3 WHERE kind = ?1 AND deleted = 0 AND id <= ?2",
            params![kind, max_id, now_micros],
        )?)
    }

    /// Tombstone every live post of one kind (each stays until the remote delete is confirmed).
    /// Returns how many.
    pub fn delete_music_posts_of_kind(&self, kind: &str, now_micros: i64) -> Result<usize> {
        Ok(self.lock().execute(
            "UPDATE music_posts SET deleted = 1, updated_at = ?2 WHERE kind = ?1 AND deleted = 0",
            params![kind, now_micros],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: &str, kind: &str, t: i64) -> MusicPostRow {
        MusicPostRow {
            id: id.into(),
            kind: kind.into(),
            visibility: "private".into(),
            json: "{}".into(),
            created_at: t,
            updated_at: t,
            synced_at: None,
            deleted: false,
        }
    }

    #[test]
    fn store_list_sync_and_tombstone() {
        let lib = Library::open_in_memory().unwrap();
        assert!(lib.schema_version().unwrap() >= 2);
        lib.upsert_music_post(&p("a", "pubky.music/playlist", 10))
            .unwrap();
        lib.upsert_music_post(&p("b", "pubky.music/playlist", 20))
            .unwrap();
        lib.upsert_music_post(&p("r", "pubky.music/review", 15)).unwrap();
        let pls = lib.list_music_posts(Some("pubky.music/playlist")).unwrap();
        assert_eq!(pls.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(lib.list_music_posts(None).unwrap().len(), 3);
        assert_eq!(lib.unsynced_music_posts().unwrap().len(), 3);

        lib.mark_music_post_synced("a", 30).unwrap();
        lib.mark_music_post_synced("b", 30).unwrap();
        lib.mark_music_post_synced("r", 30).unwrap();
        assert!(lib.unsynced_music_posts().unwrap().is_empty());
        assert_eq!(lib.get_music_post("a").unwrap().unwrap().synced_at, Some(30));

        // Edit → unsynced again; delete → tombstone until synced, then gone.
        let mut a = lib.get_music_post("a").unwrap().unwrap();
        a.updated_at = 40;
        a.json = r#"{"edited":true}"#.into();
        lib.upsert_music_post(&a).unwrap();
        assert_eq!(lib.unsynced_music_posts().unwrap()[0].id, "a");
        assert!(lib.delete_music_post("b", 50).unwrap());
        assert!(!lib.delete_music_post("b", 51).unwrap());
        assert!(lib.get_music_post("b").unwrap().is_none());
        let unsynced = lib.unsynced_music_posts().unwrap();
        assert!(unsynced.iter().any(|r| r.id == "b" && r.deleted));
        lib.mark_music_post_synced("b", 60).unwrap();
        lib.mark_music_post_unsynced("r").unwrap();
        assert_eq!(lib.unsynced_music_posts().unwrap()[0].id, "r");
        assert!(!lib.unsynced_music_posts().unwrap().iter().any(|r| r.id == "b"));
    }

    #[test]
    fn a_history_pages_moves_and_goes() {
        let lib = Library::open_in_memory().unwrap();
        let listen = |id: &str, at: i64| MusicPostRow {
            id: id.into(),
            kind: "pubky.music/activity".into(),
            visibility: "private".into(),
            json: "{}".into(),
            created_at: at,
            updated_at: at,
            synced_at: Some(at + 1),
            deleted: false,
        };
        for (id, at) in [("a", 10), ("b", 20), ("c", 30), ("d", 40)] {
            lib.upsert_music_post(&listen(id, at)).unwrap();
        }
        lib.upsert_music_post(&MusicPostRow {
            kind: "pubky.music/review".into(),
            ..listen("r", 35)
        })
        .unwrap();
        let ids = |rows: Vec<MusicPostRow>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
        let kind = "pubky.music/activity";
        assert_eq!(ids(lib.music_posts_page(kind, None, 2).unwrap()), ["d", "c"]);
        assert_eq!(ids(lib.music_posts_page(kind, Some(30), 2).unwrap()), ["b", "a"]);
        assert_eq!(ids(lib.music_posts_between(kind, 15, 30).unwrap()), ["c", "b"]);
        assert!(lib.unsynced_music_posts().unwrap().is_empty());
        assert_eq!(lib.set_music_posts_visibility(kind, "public").unwrap(), 4);
        assert_eq!(lib.set_music_posts_visibility(kind, "public").unwrap(), 0);
        assert_eq!(lib.music_post_counts(kind).unwrap(), (0, 4));
        assert_eq!(lib.unsynced_music_posts().unwrap().len(), 4, "queued to move");
        // A push that read "c" before the move doesn't mark the moved one synced
        let before_move = MusicPostRow {
            visibility: "private".into(),
            ..lib.get_music_post("c").unwrap().unwrap()
        };
        assert!(!lib.mark_music_post_synced_if_unchanged(&before_move, 50).unwrap());
        assert_eq!(
            lib.get_music_post("c").unwrap().unwrap().synced_at,
            None,
            "still to push"
        );
        let now = lib.get_music_post("c").unwrap().unwrap();
        assert!(lib.mark_music_post_synced_if_unchanged(&now, 50).unwrap());
        assert_eq!(lib.unsynced_music_posts().unwrap().len(), 3);
        // A delete on another node: the posts written up to then go
        assert_eq!(lib.delete_music_posts_of_kind_up_to(kind, "b", 60).unwrap(), 2);
        assert_eq!(ids(lib.music_posts_page(kind, None, 10).unwrap()), ["d", "c"]);
        assert_eq!(lib.delete_music_posts_of_kind(kind, 99).unwrap(), 2);
        assert!(lib.music_posts_page(kind, None, 10).unwrap().is_empty());
        assert!(lib.get_music_post("r").unwrap().is_some(), "other kinds stay");
    }
}
