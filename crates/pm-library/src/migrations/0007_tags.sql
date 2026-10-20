-- The user's own music tags: v1 PubkyAppTags on canonical music URLs, public
-- by nature (/pub/pubky.app/tags/<id>), written only on explicit action.
-- Same push/tombstone/pull bookkeeping as favourites.
CREATE TABLE tags (
    id         TEXT PRIMARY KEY,
    uri        TEXT NOT NULL,
    label      TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    synced_at  INTEGER,
    deleted    INTEGER NOT NULL DEFAULT 0,
    UNIQUE(uri, label)
);
CREATE INDEX tags_uri ON tags(uri, deleted);
