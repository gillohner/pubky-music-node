CREATE TABLE schema_version (
    version    INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE artists (
    id        INTEGER PRIMARY KEY,
    name      TEXT NOT NULL,
    mbid      TEXT,
    sort_name TEXT
);
CREATE UNIQUE INDEX artists_name_mbid ON artists(name, COALESCE(mbid, ''));
CREATE INDEX artists_mbid ON artists(mbid);

CREATE TABLE albums (
    id                 INTEGER PRIMARY KEY,
    title              TEXT NOT NULL,
    album_artist       TEXT,
    release_mbid       TEXT,
    release_group_mbid TEXT,
    year               INTEGER,
    upc                TEXT,
    total_discs        INTEGER
);
CREATE INDEX albums_release_mbid ON albums(release_mbid);
CREATE UNIQUE INDEX albums_identity ON albums(title, COALESCE(album_artist, ''), COALESCE(release_mbid, ''));

CREATE TABLE tracks (
    id                          INTEGER PRIMARY KEY,
    title                       TEXT NOT NULL,
    album_id                    INTEGER REFERENCES albums(id) ON DELETE SET NULL,
    track_no                    INTEGER,
    disc_no                     INTEGER,
    duration_ms                 INTEGER,
    recording_mbid              TEXT,
    isrc                        TEXT,
    acoustid                    TEXT,
    canonical_uri               TEXT,
    canonical_confidence_source TEXT,
    canonical_confidence_score  REAL,
    replaygain_track_gain_db    REAL,
    replaygain_album_gain_db    REAL
);
CREATE INDEX tracks_recording_mbid ON tracks(recording_mbid);
CREATE INDEX tracks_isrc ON tracks(isrc);
CREATE INDEX tracks_canonical_uri ON tracks(canonical_uri);
CREATE INDEX tracks_album ON tracks(album_id, disc_no, track_no);

CREATE TABLE track_artists (
    track_id  INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
    position  INTEGER NOT NULL,
    PRIMARY KEY (track_id, position)
);
CREATE INDEX track_artists_artist ON track_artists(artist_id);

-- One file per track in v1. A file whose path vanished keeps its row with
-- missing = 1 so playlists and history still resolve; a later scan that finds
-- it again clears the flag.
CREATE TABLE files (
    id                INTEGER PRIMARY KEY,
    track_id          INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    path              TEXT NOT NULL UNIQUE,
    size_bytes        INTEGER NOT NULL,
    mtime_unix        INTEGER NOT NULL,
    container         TEXT NOT NULL,
    sample_kind       TEXT NOT NULL,
    rate_hz           INTEGER NOT NULL,
    bits              INTEGER NOT NULL,
    channels          INTEGER NOT NULL,
    duration_ms       INTEGER,
    last_seen_scan_id INTEGER,
    missing           INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX files_track ON files(track_id);
CREATE INDEX files_missing ON files(missing);

CREATE TABLE scans (
    id            INTEGER PRIMARY KEY,
    root          TEXT NOT NULL,
    started_at    INTEGER NOT NULL,
    finished_at   INTEGER,
    files_seen    INTEGER NOT NULL DEFAULT 0,
    files_added   INTEGER NOT NULL DEFAULT 0,
    files_updated INTEGER NOT NULL DEFAULT 0,
    files_removed INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE folders (
    path    TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL DEFAULT 1
);

CREATE VIRTUAL TABLE tracks_fts USING fts5(
    track_id UNINDEXED,
    title,
    artist,
    album,
    tokenize = 'unicode61 remove_diacritics 2'
);
