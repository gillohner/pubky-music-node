-- Lyrics from the files' tags (LYRICS / UNSYNCEDLYRICS / USLT) or a sidecar
-- .lrc next to the file; one text per track, re-read on every scan.
CREATE TABLE lyrics (
    track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    text     TEXT NOT NULL,
    -- 'lrc' (sidecar file) or 'tag' (embedded)
    source   TEXT NOT NULL
);
