-- The release a fingerprint match was disambiguated to (folder as release), for writing MUSICBRAINZ_ALBUMID.
ALTER TABLE identify_state ADD COLUMN release_mbid TEXT;
