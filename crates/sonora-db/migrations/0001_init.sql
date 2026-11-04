-- Sonora library schema, phase 1.
--
-- Design notes:
--  * Text is stored as-is; matching is done through the FTS5 index so that
--    diacritics and CJK text behave sensibly.
--  * `tracks.path` is unique: one row per file. Everything else hangs off ids
--    so renaming a file does not create a duplicate album or artist.
--  * `last_seen_scan` lets an interrupted scan resume without deleting rows it
--    never reached.

CREATE TABLE library_folders (
    id         INTEGER PRIMARY KEY,
    path       TEXT    NOT NULL UNIQUE,
    added_at   INTEGER NOT NULL,
    last_scan_at INTEGER
);

CREATE TABLE artists (
    id        INTEGER PRIMARY KEY,
    name      TEXT    NOT NULL UNIQUE,
    sort_name TEXT    NOT NULL
);
CREATE INDEX artists_sort_idx ON artists (sort_name);

CREATE TABLE albums (
    id              INTEGER PRIMARY KEY,
    title           TEXT    NOT NULL,
    album_artist_id INTEGER NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
    year            INTEGER,
    UNIQUE (title, album_artist_id)
);
CREATE INDEX albums_artist_idx ON albums (album_artist_id);

CREATE TABLE genres (
    id   INTEGER PRIMARY KEY,
    name TEXT    NOT NULL UNIQUE
);

CREATE TABLE tracks (
    id             INTEGER PRIMARY KEY,
    path           TEXT    NOT NULL UNIQUE,
    folder_id      INTEGER REFERENCES library_folders (id) ON DELETE SET NULL,
    title          TEXT    NOT NULL,
    artist_id      INTEGER REFERENCES artists (id) ON DELETE SET NULL,
    album_id       INTEGER REFERENCES albums (id) ON DELETE SET NULL,
    track_no       INTEGER,
    disc_no        INTEGER,
    year           INTEGER,
    duration_ms    INTEGER NOT NULL DEFAULT 0,
    bitrate_kbps   INTEGER,
    sample_rate    INTEGER,
    channels       INTEGER,
    codec          TEXT    NOT NULL DEFAULT '',
    file_size      INTEGER NOT NULL DEFAULT 0,
    added_at       INTEGER NOT NULL,
    modified_at    INTEGER NOT NULL,
    last_seen_scan INTEGER
);
CREATE INDEX tracks_album_idx ON tracks (album_id, disc_no, track_no);
CREATE INDEX tracks_artist_idx ON tracks (artist_id);
CREATE INDEX tracks_added_idx ON tracks (added_at DESC);
CREATE INDEX tracks_title_idx ON tracks (title);

CREATE TABLE track_genres (
    track_id INTEGER NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    genre_id INTEGER NOT NULL REFERENCES genres (id) ON DELETE CASCADE,
    PRIMARY KEY (track_id, genre_id)
);

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE scan_runs (
    id          INTEGER PRIMARY KEY,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER,
    seen        INTEGER NOT NULL DEFAULT 0,
    imported    INTEGER NOT NULL DEFAULT 0,
    updated     INTEGER NOT NULL DEFAULT 0,
    removed     INTEGER NOT NULL DEFAULT 0,
    errors      INTEGER NOT NULL DEFAULT 0
);

-- Standalone (not contentless) FTS table: rows can be deleted by rowid, which
-- keeps incremental scans simple and avoids a full rebuild.
CREATE VIRTUAL TABLE tracks_fts USING fts5 (
    title,
    artist,
    album,
    album_artist,
    tokenize = 'unicode61 remove_diacritics 2'
);
