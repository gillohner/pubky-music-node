-- Small key/value store for node settings that must survive restarts
-- (per-output DSP configs and the like). Values are JSON.
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
