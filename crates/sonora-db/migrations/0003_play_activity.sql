-- Play activity: history, play/skip counts, ratings and favourites.
--
-- Two tables on purpose:
--  * `track_activity` keeps one row per track with the counters a list needs
--    without scanning the whole history. Counters are denormalised but only
--    written here, so a rescan never disturbs them.
--  * `play_history` is an append-only log. It is what the History page reads,
--    and it survives a track being renamed because it references the id.
--
-- Ratings are 0 (unrated) to 5; favourites are a flag. Both live on the
-- activity row so a favourite can exist before a track has ever been played.

CREATE TABLE track_activity (
    track_id        INTEGER PRIMARY KEY REFERENCES tracks (id) ON DELETE CASCADE,
    play_count      INTEGER NOT NULL DEFAULT 0,
    skip_count      INTEGER NOT NULL DEFAULT 0,
    rating          INTEGER NOT NULL DEFAULT 0 CHECK (rating BETWEEN 0 AND 5),
    favourite       INTEGER NOT NULL DEFAULT 0 CHECK (favourite IN (0, 1)),
    last_played_at  INTEGER,
    last_skipped_at INTEGER,
    updated_at      INTEGER NOT NULL
);

CREATE INDEX track_activity_most_played_idx
    ON track_activity (play_count DESC, last_played_at DESC);
CREATE INDEX track_activity_last_played_idx
    ON track_activity (last_played_at DESC);
CREATE INDEX track_activity_favourite_idx
    ON track_activity (favourite, last_played_at DESC);

CREATE TABLE play_history (
    id        INTEGER PRIMARY KEY,
    track_id  INTEGER NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    played_at INTEGER NOT NULL,
    skipped   INTEGER NOT NULL DEFAULT 0 CHECK (skipped IN (0, 1))
);

CREATE INDEX play_history_played_idx ON play_history (played_at DESC, id DESC);
CREATE INDEX play_history_track_idx ON play_history (track_id, played_at DESC);
