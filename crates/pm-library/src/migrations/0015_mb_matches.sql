-- MusicBrainz ids the node looked up for artists and albums (an album's is its release group).
-- Keyed by the plain name (an album: plain title and artist), not the row id, so a rescan keeps
-- them. A NULL mbid means looked up and not sure; tried again later.
CREATE TABLE mb_matches (
    kind       TEXT NOT NULL,
    key        TEXT NOT NULL,
    mbid       TEXT,
    -- 'tags' (asked by a tagged id) or 'lookup' (searched)
    source     TEXT NOT NULL,
    evidence   TEXT NOT NULL DEFAULT '',
    -- an album's credited artists, JSON [[mbid, name], ...]
    credits    TEXT NOT NULL DEFAULT '[]',
    -- what it was decided on: an album's songs, an artist's albums
    size       INTEGER NOT NULL DEFAULT 0,
    checked_at INTEGER NOT NULL,
    PRIMARY KEY (kind, key)
);
