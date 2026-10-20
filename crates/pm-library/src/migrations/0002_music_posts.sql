-- Local working copies of the user's music posts (playlists, reviews,
-- activity): the exact PubkyAppPost JSON that goes to the homeserver's
-- private folder. `synced_at` tracks the last successful push; `deleted`
-- rows are tombstones until the remote delete succeeds.
CREATE TABLE music_posts (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    visibility TEXT NOT NULL DEFAULT 'private',
    json       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    synced_at  INTEGER,
    deleted    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX music_posts_kind ON music_posts(kind, deleted, updated_at DESC);
