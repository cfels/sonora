-- Lyrics fetched from LRCLIB, one row per track.
--
-- Kept apart from `tracks` so a library rescan never rewrites fetched text, and
-- deleted with the track through the foreign key. `source` records where the
-- text came from so a future provider can coexist.

CREATE TABLE lyrics (
    track_id   INTEGER PRIMARY KEY REFERENCES tracks (id) ON DELETE CASCADE,
    plain      TEXT,
    synced     TEXT,
    source     TEXT    NOT NULL,
    fetched_at INTEGER NOT NULL
);
