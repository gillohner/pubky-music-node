-- One cover per album: a file the node wrote (embedded picture extracted at
-- scan time) or an image found next to the audio files.
CREATE TABLE album_art (
    album_id INTEGER PRIMARY KEY REFERENCES albums(id) ON DELETE CASCADE,
    file     TEXT NOT NULL,
    mime     TEXT NOT NULL,
    source   TEXT NOT NULL,          -- 'embedded' | 'folder'
    set_at   INTEGER NOT NULL
);
