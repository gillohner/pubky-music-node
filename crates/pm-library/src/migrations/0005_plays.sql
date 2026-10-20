-- Play history: one row per track start on any zone (node zones or a native
-- client's local zone that reports back). Node-local, never published.
CREATE TABLE plays (
    id         INTEGER PRIMARY KEY,
    track_id   INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    zone       TEXT,
    played_at  INTEGER NOT NULL
);
CREATE INDEX plays_track ON plays(track_id);
CREATE INDEX plays_time ON plays(played_at DESC);
