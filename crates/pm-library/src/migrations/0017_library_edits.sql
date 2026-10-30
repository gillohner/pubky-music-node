-- The owner's edits to a song's tags, kept on the node (the file is not touched) and applied
-- whenever the file is read again. `orig_*` hold what the file said when the edit was made, so
-- undoing it needs no rescan.
CREATE TABLE track_overrides (
    path              TEXT PRIMARY KEY,
    title             TEXT,
    artist            TEXT,
    album             TEXT,
    album_artist      TEXT,
    orig_title        TEXT NOT NULL,
    orig_artists      TEXT NOT NULL,
    orig_album        TEXT,
    orig_album_artist TEXT,
    updated_at        INTEGER NOT NULL
);

-- Songs removed from the library: their files stay on disk, scans skip them.
CREATE TABLE hidden_files (
    path      TEXT PRIMARY KEY,
    hidden_at INTEGER NOT NULL
);
