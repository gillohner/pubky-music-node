-- Identification of local files (steward identify brief).
-- Fingerprints are cached by a key of the file's content (see Library::file_key).
CREATE TABLE fingerprints (
    file_key      TEXT PRIMARY KEY,
    duration_secs INTEGER NOT NULL,
    fingerprint   TEXT NOT NULL,
    lookup_json   TEXT,
    looked_up_at  INTEGER
);
-- Where identification stands per track, so the job resumes.
CREATE TABLE identify_state (
    track_id   INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    state      TEXT NOT NULL,      -- identified | suggested | unknown | error
    detail     TEXT,
    checked_at INTEGER NOT NULL
);
-- Candidates for the owner to confirm; never identity on their own.
CREATE TABLE identify_suggestions (
    id             INTEGER PRIMARY KEY,
    track_id       INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    recording_mbid TEXT NOT NULL,
    release_mbid   TEXT,
    title          TEXT,
    artist         TEXT,
    release_title  TEXT,
    score          REAL NOT NULL,
    source         TEXT NOT NULL,  -- fingerprint | search
    created_at     INTEGER NOT NULL,
    UNIQUE (track_id, recording_mbid)
);
CREATE INDEX identify_suggestions_track ON identify_suggestions(track_id);
