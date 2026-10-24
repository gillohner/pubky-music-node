-- Measured loudness (EBU R128) for files whose tags carry no ReplayGain; the ReplayGain stage
-- uses it in their place. Kept with the file's size and mtime: a changed file is measured again.
CREATE TABLE track_loudness (
    track_id    INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    -- integrated loudness, LUFS
    lufs        REAL NOT NULL,
    -- sample peak, linear (1.0 = full scale)
    peak        REAL NOT NULL,
    size_bytes  INTEGER NOT NULL,
    mtime_unix  INTEGER NOT NULL,
    measured_at INTEGER NOT NULL
);
