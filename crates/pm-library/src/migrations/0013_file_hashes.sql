-- Content hashes of library files (BLAKE3 of the whole file), kept while size and mtime stay
-- the same. "Bring files here" matches the homeserver's private files (named by this hash) to
-- the library with them, so files the node already has are not downloaded again.
CREATE TABLE file_hashes (
    path TEXT PRIMARY KEY,
    size_bytes INTEGER NOT NULL,
    mtime_unix INTEGER NOT NULL,
    blake3 TEXT NOT NULL
);
CREATE INDEX file_hashes_blake3 ON file_hashes(blake3);
