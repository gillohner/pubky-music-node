-- Favourites are bookmarks (the social v1 name, owner 2026-10-26): the table and the settings
-- that hold node-only bookmarks take the name. The rows and the homeserver format
-- (/priv/social/v1/bookmarks/) do not change.
ALTER TABLE favourites RENAME TO bookmarks;
DROP INDEX IF EXISTS favourites_live;
CREATE INDEX bookmarks_live ON bookmarks(deleted, created_at DESC);
UPDATE settings SET key = 'bookmarks.local' WHERE key = 'favourites.local';
UPDATE settings SET key = 'bookmarks.local.albums' WHERE key = 'favourites.local.albums';
UPDATE settings SET key = 'bookmarks.local.artists' WHERE key = 'favourites.local.artists';
