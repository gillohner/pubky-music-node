-- The search index keyed by the track id (steward next-3: a big library stays fast). Before
-- this, `track_id` was an unindexed column, so replacing one track's entry read the whole index
-- and a first scan slowed down with every track it added (50,000 tracks: 4 min 20 s).
CREATE VIRTUAL TABLE tracks_fts_new USING fts5(
    track_id UNINDEXED,
    title,
    artist,
    album,
    tokenize = 'unicode61 remove_diacritics 2'
);
INSERT INTO tracks_fts_new(rowid, track_id, title, artist, album)
    SELECT track_id, track_id, title, artist, album FROM tracks_fts
    WHERE rowid IN (SELECT MAX(rowid) FROM tracks_fts GROUP BY track_id);
DROP TABLE tracks_fts;
ALTER TABLE tracks_fts_new RENAME TO tracks_fts;
