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
}
