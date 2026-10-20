-- Favourites ("hearts"): v1 PubkyAppBookmarks of canonical music URLs,
-- kept privately on the homeserver (/priv/pubky.app/bookmarks/<id>).
-- `synced_at` is the last successful push; `deleted` rows are tombstones
-- until the remote delete succeeds.
CREATE TABLE favourites (
    id         TEXT PRIMARY KEY,
    uri        TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    synced_at  INTEGER,
    deleted    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX favourites_live ON favourites(deleted, created_at DESC);
