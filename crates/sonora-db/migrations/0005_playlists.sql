-- Custom playlists.
--
-- Design notes:
--  * A playlist is just a name; nothing is derived, so renaming one is a single
--    UPDATE and deleting one takes its entries with it.
--  * `position` is dense (0, 1, 2, …) and owned by the reorder routine, so a
--    row's place is a number the UI can move rather than a linked list.
--  * One track appears at most once per playlist. Adding a track that is
--    already there is a no-op instead of a silent duplicate.

CREATE TABLE playlists (
    id         INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE playlist_tracks (
    playlist_id INTEGER NOT NULL REFERENCES playlists (id) ON DELETE CASCADE,
    track_id    INTEGER NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    added_at    INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, track_id)
);

CREATE INDEX playlist_tracks_position_idx ON playlist_tracks (playlist_id, position);
CREATE INDEX playlist_tracks_track_idx ON playlist_tracks (track_id);
