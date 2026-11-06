//! SQLite persistence for the Sonora library.
//!
//! The pool is the only place that knows SQL. Callers get domain types from
//! `sonora-core` back, so the UI and scanner never depend on the schema.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use sonora_core::{
    sort_key, AlbumId, Error, FolderId, LibraryStats, Result, ScanReport, ScannedTrack, Settings,
    TrackId, TrackView, QUEUE_KEY, SETTINGS_KEY,
};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePool, SqlitePoolOptions,
    SqliteRow, SqliteSynchronous,
};
use sqlx::Row;

/// Result of storing one scanned track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    Imported(TrackId),
    Updated(TrackId),
    Unchanged(TrackId),
}

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

const TRACK_COLUMNS: &str = "
    t.id, t.title, ar.name AS artist_name, al.title AS album_title,
    aar.name AS album_artist_name, t.track_no, t.disc_no, t.year,
    t.duration_ms, t.codec, t.bitrate_kbps, t.file_size, t.added_at, t.path,
    t.replay_gain_track_db, t.replay_gain_album_db,
    t.replay_gain_track_peak, t.replay_gain_album_peak,
    COALESCE(act.favourite, 0) AS favourite,
    COALESCE(act.rating, 0) AS rating,
    COALESCE(act.play_count, 0) AS play_count
";

const TRACK_FROM: &str = "
    FROM tracks t
    LEFT JOIN artists ar ON ar.id = t.artist_id
    LEFT JOIN albums al ON al.id = t.album_id
    LEFT JOIN artists aar ON aar.id = al.album_artist_id
    LEFT JOIN track_activity act ON act.track_id = t.id
";

/// Maps `sqlx` failures onto our own error type so callers can use `?`.
trait SqlxExt<T> {
    fn db(self) -> Result<T>;
}

impl<T> SqlxExt<T> for std::result::Result<T, sqlx::Error> {
    fn db(self) -> Result<T> {
        self.map_err(|err| Error::Database(err.to_string()))
    }
}

impl Db {
    /// Open (or create) the library database at `path`.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|err| Error::io(parent, err))?;
            }
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        Self::connect(options, 8).await
    }

    /// In-memory database used by tests.
    pub async fn open_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(|err| Error::Database(err.to_string()))?
            .foreign_keys(true);
        // A single connection: separate connections would each get their own
        // private in-memory database.
        Self::connect(options, 1).await
    }

    async fn connect(options: SqliteConnectOptions, max_connections: u32) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await
            .db()?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|err| Error::Database(format!("migration failed: {err}")))?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ---------------------------------------------------------------- library

    pub async fn stats(&self) -> Result<LibraryStats> {
        let row = sqlx::query(
            "SELECT
                (SELECT COUNT(*) FROM tracks) AS tracks,
                (SELECT COUNT(*) FROM albums) AS albums,
                (SELECT COUNT(*) FROM artists) AS artists,
                (SELECT COUNT(*) FROM genres) AS genres,
                (SELECT COUNT(*) FROM library_folders) AS folders,
                (SELECT COALESCE(SUM(duration_ms), 0) FROM tracks) AS total_duration_ms",
        )
        .fetch_one(&self.pool)
        .await
        .db()?;

        Ok(LibraryStats {
            tracks: row.get("tracks"),
            albums: row.get("albums"),
            artists: row.get("artists"),
            genres: row.get("genres"),
            folders: row.get("folders"),
            total_duration_ms: row.get("total_duration_ms"),
        })
    }

    /// Most recently indexed tracks — the Home page's "Recently added" shelf.
    pub async fn recently_added(&self, limit: i64) -> Result<Vec<TrackView>> {
        let sql = format!(
            "SELECT {TRACK_COLUMNS} {TRACK_FROM} ORDER BY t.added_at DESC, t.id DESC LIMIT ?1"
        );
        let rows = sqlx::query(&sql)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .db()?;
        Ok(rows.iter().map(map_track).collect())
    }

    pub async fn list_tracks(&self, limit: i64, offset: i64) -> Result<Vec<TrackView>> {
        let sql = format!(
            "SELECT {TRACK_COLUMNS} {TRACK_FROM}
             ORDER BY COALESCE(aar.sort_name, ''), COALESCE(al.title, ''),
                      t.disc_no, t.track_no, t.title
             LIMIT ?1 OFFSET ?2"
        );
        let rows = sqlx::query(&sql)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .db()?;
        Ok(rows.iter().map(map_track).collect())
    }

    pub async fn track(&self, id: TrackId) -> Result<Option<TrackView>> {
        let sql = format!("SELECT {TRACK_COLUMNS} {TRACK_FROM} WHERE t.id = ?1");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .db()?;
        Ok(row.as_ref().map(map_track))
    }

    /// Full-text search across title/artist/album, ranked by FTS5 relevance.
    pub async fn search(&self, query: &str, limit: i64) -> Result<Vec<TrackView>> {
        let Some(match_query) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT {TRACK_COLUMNS}
             FROM tracks_fts f
             JOIN tracks t ON t.id = f.rowid
             LEFT JOIN artists ar ON ar.id = t.artist_id
             LEFT JOIN albums al ON al.id = t.album_id
             LEFT JOIN artists aar ON aar.id = al.album_artist_id
             LEFT JOIN track_activity act ON act.track_id = t.id
             WHERE tracks_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2"
        );
        let rows = sqlx::query(&sql)
            .bind(match_query)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .db()?;
        Ok(rows.iter().map(map_track).collect())
    }

    pub async fn albums(&self) -> Result<Vec<AlbumSummary>> {
        let rows = sqlx::query(
            "SELECT al.id, al.title, aar.name AS album_artist, al.year,
                    COUNT(t.id) AS track_count,
                    COALESCE(SUM(t.duration_ms), 0) AS duration_ms
             FROM albums al
             JOIN artists aar ON aar.id = al.album_artist_id
             LEFT JOIN tracks t ON t.album_id = al.id
             GROUP BY al.id
             ORDER BY aar.sort_name, al.year, al.title",
        )
        .fetch_all(&self.pool)
        .await
        .db()?;
        Ok(rows
            .iter()
            .map(|row| AlbumSummary {
                id: row.get("id"),
                title: row.get("title"),
                album_artist: row.get("album_artist"),
                year: row.try_get::<Option<i64>, _>("year").unwrap_or(None),
                track_count: row.get("track_count"),
                duration_ms: row.get("duration_ms"),
            })
            .collect())
    }

    /// Every track on one album, in the order the album was recorded.
    ///
    /// Disc and track numbers come from tags, so they are missing on loose
    /// files; those sort last rather than ahead of a numbered side, which is
    /// what a plain `ORDER BY` would do with `NULL`.
    pub async fn album_tracks(&self, album: AlbumId) -> Result<Vec<TrackView>> {
        let sql = format!(
            "SELECT {TRACK_COLUMNS} {TRACK_FROM}
             WHERE t.album_id = ?1
             ORDER BY COALESCE(t.disc_no, 1), COALESCE(t.track_no, 9999), t.title"
        );
        let rows = sqlx::query(&sql)
            .bind(album)
            .fetch_all(&self.pool)
            .await
            .db()?;
        Ok(rows.iter().map(map_track).collect())
    }

    // ---------------------------------------------------------------- activity

    /// Record that playback started on `track`.
    ///
    /// A history row is appended for every start, and the counters on the
    /// activity row move with it. The two writes share a transaction so a
    /// crash cannot leave a history entry without its count.
    pub async fn record_play(&self, track: TrackId) -> Result<()> {
        let at = now();
        let mut tx = self.pool.begin().await.db()?;
        sqlx::query("INSERT INTO play_history (track_id, played_at, skipped) VALUES (?1, ?2, 0)")
            .bind(track)
            .bind(at)
            .execute(&mut *tx)
            .await
            .db()?;
        sqlx::query(
            "INSERT INTO track_activity (track_id, play_count, last_played_at, updated_at)
             VALUES (?1, 1, ?2, ?2)
             ON CONFLICT (track_id) DO UPDATE SET
                 play_count = track_activity.play_count + 1,
                 last_played_at = excluded.last_played_at,
                 updated_at = excluded.updated_at",
        )
        .bind(track)
        .bind(at)
        .execute(&mut *tx)
        .await
        .db()?;
        tx.commit().await.db()?;
        Ok(())
    }

    /// Record that the user moved away from `track` before it finished.
    ///
    /// The most recent unskipped history row for the track is marked, which is
    /// the play that was just interrupted. A skip with no preceding play (the
    /// table can only be written from the UI) still bumps the counter.
    pub async fn record_skip(&self, track: TrackId) -> Result<()> {
        let at = now();
        let mut tx = self.pool.begin().await.db()?;
        sqlx::query(
            "UPDATE play_history SET skipped = 1
             WHERE id = (
                 SELECT id FROM play_history
                 WHERE track_id = ?1 AND skipped = 0
                 ORDER BY played_at DESC, id DESC
                 LIMIT 1
             )",
        )
        .bind(track)
        .execute(&mut *tx)
        .await
        .db()?;
        sqlx::query(
            "INSERT INTO track_activity (track_id, skip_count, last_skipped_at, updated_at)
             VALUES (?1, 1, ?2, ?2)
             ON CONFLICT (track_id) DO UPDATE SET
                 skip_count = track_activity.skip_count + 1,
                 last_skipped_at = excluded.last_skipped_at,
                 updated_at = excluded.updated_at",
        )
        .bind(track)
        .bind(at)
        .execute(&mut *tx)
        .await
        .db()?;
        tx.commit().await.db()?;
        Ok(())
    }

    /// Set a star rating. `0` clears it; values above five are clamped.
    pub async fn set_rating(&self, track: TrackId, rating: u8) -> Result<()> {
        let rating = i64::from(rating.min(5));
        sqlx::query(
            "INSERT INTO track_activity (track_id, rating, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (track_id) DO UPDATE SET
                 rating = excluded.rating,
                 updated_at = excluded.updated_at",
        )
        .bind(track)
        .bind(rating)
        .bind(now())
        .execute(&self.pool)
        .await
        .db()?;
        Ok(())
    }

    /// Star or unstar a track.
    pub async fn set_favourite(&self, track: TrackId, favourite: bool) -> Result<()> {
        sqlx::query(
            "INSERT INTO track_activity (track_id, favourite, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (track_id) DO UPDATE SET
                 favourite = excluded.favourite,
                 updated_at = excluded.updated_at",
        )
        .bind(track)
        .bind(if favourite { 1i64 } else { 0 })
        .bind(now())
        .execute(&self.pool)
        .await
        .db()?;
        Ok(())
    }

    /// Recent plays, newest first. Each row carries the track as it looks now,
    /// so editing a tag updates every historical entry too.
    pub async fn history(&self, limit: i64, offset: i64) -> Result<Vec<HistoryEntry>> {
        let sql = format!(
            "SELECT h.id AS history_id, h.played_at AS history_played_at,
                    h.skipped AS history_skipped, {TRACK_COLUMNS}
             FROM play_history h
             JOIN tracks t ON t.id = h.track_id
             LEFT JOIN artists ar ON ar.id = t.artist_id
             LEFT JOIN albums al ON al.id = t.album_id
             LEFT JOIN artists aar ON aar.id = al.album_artist_id
             LEFT JOIN track_activity act ON act.track_id = t.id
             ORDER BY h.played_at DESC, h.id DESC
             LIMIT ?1 OFFSET ?2"
        );
        let rows = sqlx::query(&sql)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .db()?;
        Ok(rows
            .iter()
            .map(|row| HistoryEntry {
                id: row.get("history_id"),
                played_at: row.get("history_played_at"),
                skipped: row.get::<i64, _>("history_skipped") != 0,
                track: map_track(row),
            })
            .collect())
    }

    /// Starred tracks, most recently played first.
    pub async fn favourites(&self, limit: i64) -> Result<Vec<TrackView>> {
        let sql = format!(
            "SELECT {TRACK_COLUMNS} {TRACK_FROM}
             WHERE act.favourite = 1
             ORDER BY COALESCE(act.last_played_at, 0) DESC, t.title
             LIMIT ?1"
        );
        let rows = sqlx::query(&sql)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .db()?;
        Ok(rows.iter().map(map_track).collect())
    }

    /// Tracks ordered by how often playback started on them.
    pub async fn most_played(&self, limit: i64) -> Result<Vec<TrackView>> {
        let sql = format!(
            "SELECT {TRACK_COLUMNS} {TRACK_FROM}
             WHERE act.play_count > 0
             ORDER BY act.play_count DESC, COALESCE(act.last_played_at, 0) DESC, t.title
             LIMIT ?1"
        );
        let rows = sqlx::query(&sql)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .db()?;
        Ok(rows.iter().map(map_track).collect())
    }

    /// Distinct tracks ordered by their last play, for the Home shelf.
    pub async fn recently_played(&self, limit: i64) -> Result<Vec<TrackView>> {
        let sql = format!(
            "SELECT {TRACK_COLUMNS} {TRACK_FROM}
             WHERE act.last_played_at IS NOT NULL
             ORDER BY act.last_played_at DESC
             LIMIT ?1"
        );
        let rows = sqlx::query(&sql)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .db()?;
        Ok(rows.iter().map(map_track).collect())
    }

    // ------------------------------------------------------------- scan write

    pub async fn add_folder(&self, path: &Path) -> Result<FolderId> {
        let path = path.to_string_lossy().to_string();
        sqlx::query("INSERT OR IGNORE INTO library_folders (path, added_at) VALUES (?1, ?2)")
            .bind(&path)
            .bind(now())
            .execute(&self.pool)
            .await
            .db()?;
        let row = sqlx::query("SELECT id FROM library_folders WHERE path = ?1")
            .bind(&path)
            .fetch_one(&self.pool)
            .await
            .db()?;
        Ok(row.get("id"))
    }

    pub async fn folders(&self) -> Result<Vec<(FolderId, PathBuf)>> {
        let rows = sqlx::query("SELECT id, path FROM library_folders ORDER BY path")
            .fetch_all(&self.pool)
            .await
            .db()?;
        Ok(rows
            .iter()
            .map(|row| {
                (
                    row.get::<i64, _>("id"),
                    PathBuf::from(row.get::<String, _>("path")),
                )
            })
            .collect())
    }

    pub async fn remove_folder(&self, id: FolderId) -> Result<()> {
        sqlx::query("DELETE FROM library_folders WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .db()?;
        Ok(())
    }

    pub async fn begin_scan(&self) -> Result<i64> {
        let result = sqlx::query("INSERT INTO scan_runs (started_at) VALUES (?1)")
            .bind(now())
            .execute(&self.pool)
            .await
            .db()?;
        Ok(result.last_insert_rowid())
    }

    pub async fn finish_scan(
        &self,
        run_id: i64,
        folder_id: FolderId,
        report: &ScanReport,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.db()?;
        sqlx::query(
            "UPDATE scan_runs SET finished_at = ?1, seen = ?2, imported = ?3,
                    updated = ?4, removed = ?5, errors = ?6 WHERE id = ?7",
        )
        .bind(now())
        .bind(report.seen as i64)
        .bind(report.imported as i64)
        .bind(report.updated as i64)
        .bind(report.removed as i64)
        .bind(report.total_errors() as i64)
        .bind(run_id)
        .execute(&mut *tx)
        .await
        .db()?;
        sqlx::query("UPDATE library_folders SET last_scan_at = ?1 WHERE id = ?2")
            .bind(now())
            .bind(folder_id)
            .execute(&mut *tx)
            .await
            .db()?;
        tx.commit().await.db()?;
        Ok(())
    }

    /// Store a batch of scanned tracks in one transaction.
    pub async fn upsert_scanned(
        &self,
        folder_id: FolderId,
        run_id: i64,
        tracks: &[ScannedTrack],
    ) -> Result<Vec<UpsertOutcome>> {
        let mut tx = self.pool.begin().await.db()?;
        let mut outcomes = Vec::with_capacity(tracks.len());

        for track in tracks {
            let path = track.path.to_string_lossy().to_string();
            let existing =
                sqlx::query("SELECT id, file_size, modified_at FROM tracks WHERE path = ?1")
                    .bind(&path)
                    .fetch_optional(&mut *tx)
                    .await
                    .db()?;

            let artist_id = match track.artist.as_deref() {
                Some(name) => Some(get_or_create_artist(&mut tx, name).await?),
                None => None,
            };

            let album_artist_name = track
                .effective_album_artist()
                .unwrap_or("Unknown Artist")
                .to_string();
            let album_artist_id = get_or_create_artist(&mut tx, &album_artist_name).await?;

            let album_id = if track.album.is_some() {
                Some(
                    get_or_create_album(
                        &mut tx,
                        track.effective_album(),
                        album_artist_id,
                        track.year,
                    )
                    .await?,
                )
            } else {
                None
            };

            let track_id = match &existing {
                Some(row) => {
                    let id: i64 = row.get("id");
                    let same_size: i64 = row.get("file_size");
                    let same_mtime: i64 = row.get("modified_at");
                    if same_size == track.file_size as i64 && same_mtime == track.modified_at {
                        sqlx::query("UPDATE tracks SET last_seen_scan = ?1 WHERE id = ?2")
                            .bind(run_id)
                            .bind(id)
                            .execute(&mut *tx)
                            .await
                            .db()?;
                        outcomes.push(UpsertOutcome::Unchanged(id));
                        continue;
                    }
                    update_track(&mut tx, id, run_id, folder_id, track, artist_id, album_id)
                        .await?;
                    outcomes.push(UpsertOutcome::Updated(id));
                    id
                }
                None => {
                    let id = insert_track(&mut tx, run_id, folder_id, track, artist_id, album_id)
                        .await?;
                    outcomes.push(UpsertOutcome::Imported(id));
                    id
                }
            };

            // Genres and the search index are cheap to rebuild per track.
            sqlx::query("DELETE FROM track_genres WHERE track_id = ?1")
                .bind(track_id)
                .execute(&mut *tx)
                .await
                .db()?;
            for genre in &track.genres {
                let genre_id = get_or_create_genre(&mut tx, genre).await?;
                sqlx::query(
                    "INSERT OR IGNORE INTO track_genres (track_id, genre_id) VALUES (?1, ?2)",
                )
                .bind(track_id)
                .bind(genre_id)
                .execute(&mut *tx)
                .await
                .db()?;
            }

            sqlx::query("DELETE FROM tracks_fts WHERE rowid = ?1")
                .bind(track_id)
                .execute(&mut *tx)
                .await
                .db()?;
            sqlx::query(
                "INSERT INTO tracks_fts (rowid, title, artist, album, album_artist)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(track_id)
            .bind(&track.title)
            .bind(track.artist.as_deref().unwrap_or(""))
            .bind(track.album.as_deref().unwrap_or(""))
            .bind(&album_artist_name)
            .execute(&mut *tx)
            .await
            .db()?;
        }

        tx.commit().await.db()?;
        Ok(outcomes)
    }

    /// Delete rows for this folder that the latest scan did not touch, then
    /// drop albums/artists left with no tracks.
    pub async fn prune_missing(&self, folder_id: FolderId, run_id: i64) -> Result<u64> {
        let mut tx = self.pool.begin().await.db()?;
        let stale = sqlx::query(
            "SELECT id FROM tracks WHERE folder_id = ?1 AND (last_seen_scan IS NULL OR last_seen_scan <> ?2)",
        )
        .bind(folder_id)
        .bind(run_id)
        .fetch_all(&mut *tx)
        .await
        .db()?;

        for row in &stale {
            let id: i64 = row.get("id");
            sqlx::query("DELETE FROM tracks_fts WHERE rowid = ?1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .db()?;
            sqlx::query("DELETE FROM tracks WHERE id = ?1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .db()?;
        }

        sqlx::query("DELETE FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL)")
            .execute(&mut *tx)
            .await
            .db()?;
        sqlx::query("DELETE FROM artists WHERE id NOT IN (SELECT artist_id FROM tracks WHERE artist_id IS NOT NULL) AND id NOT IN (SELECT album_artist_id FROM albums)")
            .execute(&mut *tx)
            .await
            .db()?;

        tx.commit().await.db()?;
        Ok(stale.len() as u64)
    }

    // ----------------------------------------------------------------- lyrics

    /// Store lyrics for a track, replacing whatever was there before.
    pub async fn save_lyrics(
        &self,
        track: TrackId,
        plain: Option<&str>,
        synced: Option<&str>,
        source: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO lyrics (track_id, plain, synced, source, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (track_id) DO UPDATE SET
                 plain = excluded.plain,
                 synced = excluded.synced,
                 source = excluded.source,
                 fetched_at = excluded.fetched_at",
        )
        .bind(track)
        .bind(plain)
        .bind(synced)
        .bind(source)
        .bind(now())
        .execute(&self.pool)
        .await
        .db()?;
        Ok(())
    }

    pub async fn lyrics_for(&self, track: TrackId) -> Result<Option<StoredLyrics>> {
        let row =
            sqlx::query("SELECT plain, synced, source, fetched_at FROM lyrics WHERE track_id = ?1")
                .bind(track)
                .fetch_optional(&self.pool)
                .await
                .db()?;
        Ok(row.map(|row| StoredLyrics {
            plain: opt_string(&row, "plain"),
            synced: opt_string(&row, "synced"),
            source: row.get("source"),
            fetched_at: row.get("fetched_at"),
        }))
    }

    pub async fn clear_lyrics(&self, track: TrackId) -> Result<()> {
        sqlx::query("DELETE FROM lyrics WHERE track_id = ?1")
            .bind(track)
            .execute(&self.pool)
            .await
            .db()?;
        Ok(())
    }

    // ---------------------------------------------------------------- settings

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .db()?;
        Ok(())
    }

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = ?1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .db()?;
        Ok(row.map(|row| row.get::<String, _>("value")))
    }

    /// Read the settings document.
    ///
    /// A missing document means "first run"; an unreadable one means someone
    /// edited the database by hand, so log it and start from defaults rather
    /// than refusing to open the application.
    pub async fn load_settings(&self) -> Result<Settings> {
        let Some(raw) = self.get_setting(SETTINGS_KEY).await? else {
            return Ok(Settings::default());
        };
        match Settings::from_json(&raw) {
            Ok(settings) => Ok(settings),
            Err(err) => {
                log::warn!("ignoring unreadable settings document: {err}");
                Ok(Settings::default())
            }
        }
    }

    /// Persist settings, clamping them into range first so the database only
    /// ever holds valid values.
    pub async fn save_settings(&self, settings: &Settings) -> Result<()> {
        let document = settings.clone().normalized().to_json();
        self.set_setting(SETTINGS_KEY, &document).await
    }

    /// Read the saved playback queue.
    ///
    /// A missing document means no queue has been saved yet; an unreadable one
    /// means the document was corrupted, so log it and return a fresh empty
    /// queue instead of refusing to open the application.
    ///
    /// The RNG state is preserved through the empty path so a restart does not
    /// reseed a previously checkpointed shuffle.
    pub async fn load_queue(&self) -> Result<sonora_core::Queue> {
        let Some(raw) = self.get_setting(QUEUE_KEY).await? else {
            return Ok(sonora_core::Queue::from_empty_snapshot(
                sonora_core::util::new_rng(),
            ));
        };
        match sonora_core::QueueSnapshot::from_json(&raw) {
            Ok(snapshot) => Ok(sonora_core::Queue::from_snapshot(snapshot)),
            Err(err) => {
                log::warn!("ignoring unreadable queue document: {err}");
                Ok(sonora_core::Queue::from_empty_snapshot(
                    sonora_core::util::new_rng(),
                ))
            }
        }
    }

    /// Persist the current playback queue.
    ///
    /// Called on shutdown and whenever the queue changes in a way that should
    /// survive a restart (played, added, removed, reordered, shuffle toggled).
    /// A missing document means no queue has been saved yet; an unreadable one
    /// means the document was corrupted, so log it and write fresh defaults
    /// instead of leaving a broken document in the database.
    pub async fn save_queue(&self, queue: &sonora_core::Queue) -> Result<()> {
        let snapshot = queue.snapshot();
        match snapshot.to_json() {
            Ok(document) => self.set_setting(QUEUE_KEY, &document).await,
            Err(err) => {
                log::warn!("could not serialise the queue: {err}");
                Ok(())
            }
        }
    }

    /// Persist a queue snapshot directly.
    ///
    /// Used by the UI layer, which holds the engine (not the queue) and can
    /// only get a snapshot out.
    pub async fn save_queue_snapshot(&self, snapshot: &sonora_core::QueueSnapshot) -> Result<()> {
        match snapshot.to_json() {
            Ok(document) => self.set_setting(QUEUE_KEY, &document).await,
            Err(err) => {
                log::warn!("could not serialise the queue: {err}");
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLyrics {
    pub plain: Option<String>,
    pub synced: Option<String>,
    pub source: String,
    pub fetched_at: i64,
}

impl StoredLyrics {
    /// The text to render when the view cannot follow timestamps.
    pub fn display_text(&self) -> Option<&str> {
        self.plain.as_deref().or(self.synced.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumSummary {
    pub id: AlbumId,
    pub title: String,
    pub album_artist: String,
    pub year: Option<i64>,
    pub track_count: i64,
    pub duration_ms: i64,
}

/// One row on the History page: a play event plus the track it refers to.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub id: i64,
    pub played_at: i64,
    pub skipped: bool,
    pub track: TrackView,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn opt_string(row: &SqliteRow, column: &str) -> Option<String> {
    row.try_get::<Option<String>, _>(column).unwrap_or(None)
}

fn map_track(row: &SqliteRow) -> TrackView {
    TrackView {
        id: row.get("id"),
        title: row.get("title"),
        artist: opt_string(row, "artist_name"),
        album: opt_string(row, "album_title"),
        album_artist: opt_string(row, "album_artist_name"),
        track_no: row.try_get::<Option<i64>, _>("track_no").unwrap_or(None),
        disc_no: row.try_get::<Option<i64>, _>("disc_no").unwrap_or(None),
        year: row.try_get::<Option<i64>, _>("year").unwrap_or(None),
        duration_ms: row.get("duration_ms"),
        codec: row.get("codec"),
        bitrate_kbps: row
            .try_get::<Option<i64>, _>("bitrate_kbps")
            .unwrap_or(None),
        file_size: row.get("file_size"),
        added_at: row.get("added_at"),
        path: PathBuf::from(row.get::<String, _>("path")),
        favourite: row.get::<i64, _>("favourite") != 0,
        rating: row.get("rating"),
        play_count: row.get("play_count"),
        replay_gain_track_db: row
            .try_get::<Option<f64>, _>("replay_gain_track_db")
            .unwrap_or(None),
        replay_gain_album_db: row
            .try_get::<Option<f64>, _>("replay_gain_album_db")
            .unwrap_or(None),
        replay_gain_track_peak: row
            .try_get::<Option<f64>, _>("replay_gain_track_peak")
            .unwrap_or(None),
        replay_gain_album_peak: row
            .try_get::<Option<f64>, _>("replay_gain_album_peak")
            .unwrap_or(None),
    }
}

/// Turn a user query into a safe FTS5 expression.
///
/// Every token is quoted (so punctuation cannot inject query syntax) and gets a
/// prefix `*`, which makes incremental typing match artists and albums early.
fn fts_query(input: &str) -> Option<String> {
    let tokens: Vec<String> = input
        .split_whitespace()
        .filter(|token| !token.is_empty())
        .map(|token| {
            let escaped = token.replace('"', "\"\"");
            format!("\"{escaped}\"*")
        })
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" AND "))
    }
}

async fn get_or_create_artist(conn: &mut SqliteConnection, name: &str) -> Result<i64> {
    let name = name.trim();
    if let Some(id) = sqlx::query_scalar::<_, i64>("SELECT id FROM artists WHERE name = ?1")
        .bind(name)
        .fetch_optional(&mut *conn)
        .await
        .db()?
    {
        return Ok(id);
    }
    sqlx::query("INSERT INTO artists (name, sort_name) VALUES (?1, ?2)")
        .bind(name)
        .bind(sort_key(name))
        .execute(&mut *conn)
        .await
        .db()?;
    sqlx::query_scalar::<_, i64>("SELECT id FROM artists WHERE name = ?1")
        .bind(name)
        .fetch_one(&mut *conn)
        .await
        .db()
}

async fn get_or_create_album(
    conn: &mut SqliteConnection,
    title: &str,
    album_artist_id: i64,
    year: Option<i32>,
) -> Result<i64> {
    if let Some(id) = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM albums WHERE title = ?1 AND album_artist_id = ?2",
    )
    .bind(title)
    .bind(album_artist_id)
    .fetch_optional(&mut *conn)
    .await
    .db()?
    {
        return Ok(id);
    }
    sqlx::query("INSERT INTO albums (title, album_artist_id, year) VALUES (?1, ?2, ?3)")
        .bind(title)
        .bind(album_artist_id)
        .bind(year)
        .execute(&mut *conn)
        .await
        .db()?;
    sqlx::query_scalar::<_, i64>("SELECT id FROM albums WHERE title = ?1 AND album_artist_id = ?2")
        .bind(title)
        .bind(album_artist_id)
        .fetch_one(&mut *conn)
        .await
        .db()
}

async fn get_or_create_genre(conn: &mut SqliteConnection, name: &str) -> Result<i64> {
    let name = name.trim();
    if let Some(id) = sqlx::query_scalar::<_, i64>("SELECT id FROM genres WHERE name = ?1")
        .bind(name)
        .fetch_optional(&mut *conn)
        .await
        .db()?
    {
        return Ok(id);
    }
    sqlx::query("INSERT INTO genres (name) VALUES (?1)")
        .bind(name)
        .execute(&mut *conn)
        .await
        .db()?;
    sqlx::query_scalar::<_, i64>("SELECT id FROM genres WHERE name = ?1")
        .bind(name)
        .fetch_one(&mut *conn)
        .await
        .db()
}

async fn insert_track(
    conn: &mut SqliteConnection,
    run_id: i64,
    folder_id: FolderId,
    track: &ScannedTrack,
    artist_id: Option<i64>,
    album_id: Option<i64>,
) -> Result<i64> {
    let result = sqlx::query(
        "INSERT INTO tracks
            (path, folder_id, title, artist_id, album_id, track_no, disc_no, year,
             duration_ms, bitrate_kbps, sample_rate, channels, codec, file_size,
             added_at, modified_at, last_seen_scan,
             replay_gain_track_db, replay_gain_album_db,
             replay_gain_track_peak, replay_gain_album_peak)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18, ?19, ?20, ?21)",
    )
    .bind(track.path.to_string_lossy().to_string())
    .bind(folder_id)
    .bind(&track.title)
    .bind(artist_id)
    .bind(album_id)
    .bind(track.track_no.map(|value| value as i64))
    .bind(track.disc_no.map(|value| value as i64))
    .bind(track.year.map(|value| value as i64))
    .bind(track.duration_ms as i64)
    .bind(track.bitrate_kbps.map(|value| value as i64))
    .bind(track.sample_rate.map(|value| value as i64))
    .bind(track.channels.map(|value| value as i64))
    .bind(&track.codec)
    .bind(track.file_size as i64)
    .bind(now())
    .bind(track.modified_at)
    .bind(run_id)
    .bind(track.replay_gain_track_db)
    .bind(track.replay_gain_album_db)
    .bind(track.replay_gain_track_peak)
    .bind(track.replay_gain_album_peak)
    .execute(&mut *conn)
    .await
    .db()?;
    Ok(result.last_insert_rowid())
}

async fn update_track(
    conn: &mut SqliteConnection,
    id: i64,
    run_id: i64,
    folder_id: FolderId,
    track: &ScannedTrack,
    artist_id: Option<i64>,
    album_id: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "UPDATE tracks SET
            folder_id = ?2, title = ?3, artist_id = ?4, album_id = ?5, track_no = ?6,
            disc_no = ?7, year = ?8, duration_ms = ?9, bitrate_kbps = ?10,
            sample_rate = ?11, channels = ?12, codec = ?13, file_size = ?14,
            modified_at = ?15, last_seen_scan = ?16,
            replay_gain_track_db = ?17, replay_gain_album_db = ?18,
            replay_gain_track_peak = ?19, replay_gain_album_peak = ?20
         WHERE id = ?1",
    )
    .bind(id)
    .bind(folder_id)
    .bind(&track.title)
    .bind(artist_id)
    .bind(album_id)
    .bind(track.track_no.map(|value| value as i64))
    .bind(track.disc_no.map(|value| value as i64))
    .bind(track.year.map(|value| value as i64))
    .bind(track.duration_ms as i64)
    .bind(track.bitrate_kbps.map(|value| value as i64))
    .bind(track.sample_rate.map(|value| value as i64))
    .bind(track.channels.map(|value| value as i64))
    .bind(&track.codec)
    .bind(track.file_size as i64)
    .bind(track.modified_at)
    .bind(run_id)
    .bind(track.replay_gain_track_db)
    .bind(track.replay_gain_album_db)
    .bind(track.replay_gain_track_peak)
    .bind(track.replay_gain_album_peak)
    .execute(&mut *conn)
    .await
    .db()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(path: &str, title: &str, artist: &str, album: &str) -> ScannedTrack {
        ScannedTrack {
            path: PathBuf::from(path),
            title: title.to_string(),
            artist: Some(artist.to_string()),
            album_artist: Some(artist.to_string()),
            album: Some(album.to_string()),
            genres: vec!["Electronic".to_string()],
            track_no: Some(1),
            disc_no: Some(1),
            year: Some(1999),
            duration_ms: 180_000,
            bitrate_kbps: Some(320),
            sample_rate: Some(44_100),
            channels: Some(2),
            codec: "MP3".to_string(),
            file_size: 1024,
            modified_at: 1_700_000_000,
            replay_gain_track_db: None,
            replay_gain_album_db: None,
            replay_gain_track_peak: None,
            replay_gain_album_peak: None,
        }
    }

    #[tokio::test]
    async fn stores_and_counts_tracks() {
        let db = Db::open_in_memory().await.unwrap();
        let folder = db.add_folder(Path::new("/music")).await.unwrap();
        let run = db.begin_scan().await.unwrap();

        let outcomes = db
            .upsert_scanned(
                folder,
                run,
                &[
                    track("/music/a.mp3", "Windowlicker", "Aphex Twin", "Windowlicker"),
                    track(
                        "/music/b.mp3",
                        "Xtal",
                        "Aphex Twin",
                        "Selected Ambient Works",
                    ),
                ],
            )
            .await
            .unwrap();
        assert!(matches!(outcomes[0], UpsertOutcome::Imported(_)));

        let stats = db.stats().await.unwrap();
        assert_eq!(stats.tracks, 2);
        assert_eq!(stats.albums, 2);
        assert_eq!(stats.artists, 1);
        assert_eq!(stats.genres, 1);
        assert_eq!(stats.total_duration_ms, 360_000);
    }

    #[tokio::test]
    async fn replay_gain_survives_a_round_trip() {
        let db = Db::open_in_memory().await.unwrap();
        let folder = db.add_folder(Path::new("/music")).await.unwrap();
        let run = db.begin_scan().await.unwrap();
        let mut item = track("/music/g.mp3", "Xtal", "Aphex Twin", "SAW");
        item.replay_gain_track_db = Some(-6.25);
        item.replay_gain_album_db = Some(-4.5);
        item.replay_gain_track_peak = Some(0.875);
        item.replay_gain_album_peak = Some(0.75);
        db.upsert_scanned(folder, run, std::slice::from_ref(&item))
            .await
            .unwrap();

        let stored = &db.list_tracks(10, 0).await.unwrap()[0];
        assert_eq!(stored.replay_gain_track_db, Some(-6.25));
        assert_eq!(stored.replay_gain_album_db, Some(-4.5));
        assert_eq!(stored.replay_gain_track_peak, Some(0.875));
        assert_eq!(stored.replay_gain_album_peak, Some(0.75));
        // The engine applies the track gain when a file carries both values.
        assert_eq!(stored.effective_replay_gain_db(), Some(-6.25));
    }

    #[tokio::test]
    async fn album_tracks_lists_one_album_in_track_order() {
        let db = Db::open_in_memory().await.unwrap();
        let folder = db.add_folder(Path::new("/music")).await.unwrap();
        let run = db.begin_scan().await.unwrap();

        let mut first = track("/music/1.mp3", "Xtal", "Aphex Twin", "SAW 85-92");
        first.track_no = Some(1);
        let mut second = track("/music/2.mp3", "Tha", "Aphex Twin", "SAW 85-92");
        second.track_no = Some(2);
        // A file with no track number must sort after the numbered ones.
        let mut loose = track("/music/3.mp3", "Untitled", "Aphex Twin", "SAW 85-92");
        loose.track_no = None;
        let elsewhere = track(
            "/music/9.mp3",
            "Roygbiv",
            "Boards of Canada",
            "Music Has the Right",
        );

        // Inserted out of order on purpose: the query, not the scan, orders.
        db.upsert_scanned(folder, run, &[second, first, loose, elsewhere])
            .await
            .unwrap();

        let album = db
            .albums()
            .await
            .unwrap()
            .into_iter()
            .find(|album| album.title == "SAW 85-92")
            .expect("album");
        assert_eq!(album.track_count, 3);

        let titles: Vec<String> = db
            .album_tracks(album.id)
            .await
            .unwrap()
            .into_iter()
            .map(|track| track.title)
            .collect();
        assert_eq!(titles, vec!["Xtal", "Tha", "Untitled"]);

        assert!(db.album_tracks(9_999).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rescanning_same_file_is_unchanged() {
        let db = Db::open_in_memory().await.unwrap();
        let folder = db.add_folder(Path::new("/music")).await.unwrap();
        let run = db.begin_scan().await.unwrap();
        let item = track("/music/a.mp3", "Xtal", "Aphex Twin", "SAW");
        db.upsert_scanned(folder, run, std::slice::from_ref(&item))
            .await
            .unwrap();

        let run2 = db.begin_scan().await.unwrap();
        let outcomes = db.upsert_scanned(folder, run2, &[item]).await.unwrap();
        assert!(matches!(outcomes[0], UpsertOutcome::Unchanged(_)));
        assert_eq!(db.stats().await.unwrap().tracks, 1);
    }

    #[tokio::test]
    async fn prune_removes_unseen_tracks_and_orphans() {
        let db = Db::open_in_memory().await.unwrap();
        let folder = db.add_folder(Path::new("/music")).await.unwrap();
        let run = db.begin_scan().await.unwrap();
        db.upsert_scanned(
            folder,
            run,
            &[
                track("/music/a.mp3", "Xtal", "Aphex Twin", "SAW"),
                track(
                    "/music/b.mp3",
                    "Roygbiv",
                    "Boards of Canada",
                    "Music Has the Right",
                ),
            ],
        )
        .await
        .unwrap();

        let run2 = db.begin_scan().await.unwrap();
        db.upsert_scanned(
            folder,
            run2,
            &[track("/music/a.mp3", "Xtal", "Aphex Twin", "SAW")],
        )
        .await
        .unwrap();
        let removed = db.prune_missing(folder, run2).await.unwrap();

        assert_eq!(removed, 1);
        let stats = db.stats().await.unwrap();
        assert_eq!(stats.tracks, 1);
        assert_eq!(stats.artists, 1);
        assert_eq!(stats.albums, 1);
    }

    #[tokio::test]
    async fn search_matches_prefix_and_is_injection_safe() {
        let db = Db::open_in_memory().await.unwrap();
        let folder = db.add_folder(Path::new("/music")).await.unwrap();
        let run = db.begin_scan().await.unwrap();
        db.upsert_scanned(
            folder,
            run,
            &[
                track("/music/a.mp3", "Windowlicker", "Aphex Twin", "Windowlicker"),
                track("/music/b.mp3", "Xtal", "Aphex Twin", "SAW 85-92"),
            ],
        )
        .await
        .unwrap();

        let hits = db.search("aphex", 10).await.unwrap();
        assert_eq!(hits.len(), 2);

        let hits = db.search("window", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Windowlicker");

        // Quotes and operators must be treated as literal text, not syntax.
        assert!(db.search("\" OR 1=1 --", 10).await.is_ok());
        assert!(db.search("   ", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn lyrics_replace_instead_of_duplicating() {
        let db = Db::open_in_memory().await.unwrap();
        let folder = db.add_folder(Path::new("/music")).await.unwrap();
        let run = db.begin_scan().await.unwrap();
        let id = match db
            .upsert_scanned(
                folder,
                run,
                &[track("/music/a.mp3", "Xtal", "Aphex Twin", "SAW")],
            )
            .await
            .unwrap()[0]
        {
            UpsertOutcome::Imported(id) => id,
            other => panic!("expected an import, got {other:?}"),
        };

        assert_eq!(db.lyrics_for(id).await.unwrap(), None);

        db.save_lyrics(id, Some("first pass"), None, "lrclib")
            .await
            .unwrap();
        db.save_lyrics(id, Some("second pass"), Some("[00:01.00] second"), "lrclib")
            .await
            .unwrap();

        let stored = db.lyrics_for(id).await.unwrap().expect("lyrics");
        assert_eq!(stored.plain.as_deref(), Some("second pass"));
        assert_eq!(stored.display_text(), Some("second pass"));
        assert_eq!(stored.source, "lrclib");
        assert!(stored.fetched_at > 0);
    }

    #[tokio::test]
    async fn removing_a_track_removes_its_lyrics() {
        let db = Db::open_in_memory().await.unwrap();
        let folder = db.add_folder(Path::new("/music")).await.unwrap();
        let run = db.begin_scan().await.unwrap();
        let id = match db
            .upsert_scanned(
                folder,
                run,
                &[track("/music/a.mp3", "Xtal", "Aphex Twin", "SAW")],
            )
            .await
            .unwrap()[0]
        {
            UpsertOutcome::Imported(id) => id,
            other => panic!("expected an import, got {other:?}"),
        };
        db.save_lyrics(id, Some("words"), None, "lrclib")
            .await
            .unwrap();

        sqlx::query("DELETE FROM tracks WHERE id = ?1")
            .bind(id)
            .execute(db.pool())
            .await
            .unwrap();

        assert_eq!(db.lyrics_for(id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn settings_round_trip() {
        let db = Db::open_in_memory().await.unwrap();
        assert_eq!(db.get_setting("theme").await.unwrap(), None);
        db.set_setting("theme", "dark").await.unwrap();
        db.set_setting("theme", "light").await.unwrap();
        assert_eq!(
            db.get_setting("theme").await.unwrap(),
            Some("light".to_string())
        );
    }

    #[tokio::test]
    async fn settings_default_when_absent_and_persist_when_saved() {
        let db = Db::open_in_memory().await.unwrap();
        assert_eq!(db.load_settings().await.unwrap(), Settings::default());

        let settings = Settings {
            format: sonora_core::FormatPreference::new(sonora_core::FormatTier::Lossy, 256),
            // Below the minimum, so saving must clamp it.
            cache_limit_bytes: 1,
            ..Settings::default()
        };
        db.save_settings(&settings).await.unwrap();

        let loaded = db.load_settings().await.unwrap();
        assert_eq!(loaded.format.tier, sonora_core::FormatTier::Lossy);
        assert_eq!(loaded.format.max_bitrate_kbps, 256);
        assert_eq!(
            loaded.cache_limit_bytes,
            sonora_core::settings::MIN_CACHE_LIMIT_BYTES
        );
    }

    #[tokio::test]
    async fn unreadable_settings_fall_back_to_defaults() {
        let db = Db::open_in_memory().await.unwrap();
        db.set_setting(SETTINGS_KEY, "{ not json").await.unwrap();
        assert_eq!(db.load_settings().await.unwrap(), Settings::default());
    }

    #[tokio::test]
    async fn queue_default_is_empty_on_first_run() {
        let db = Db::open_in_memory().await.unwrap();
        let queue = db.load_queue().await.unwrap();
        assert!(queue.is_empty());
        assert_eq!(queue.current(), None);
    }

    #[tokio::test]
    async fn queue_round_trip_persists_the_full_state() {
        let db = Db::open_in_memory().await.unwrap();
        let mut queue = sonora_core::Queue::from_tracks([1, 2, 3]);
        queue.play_next(99);
        queue.set_repeat(sonora_core::RepeatMode::All);
        queue.set_shuffle(true);
        db.save_queue(&queue).await.unwrap();

        let loaded = db.load_queue().await.unwrap();
        assert_eq!(loaded.current(), queue.current());
        assert_eq!(loaded.upcoming_tracks(), queue.upcoming_tracks());
        assert_eq!(loaded.history_tracks(), queue.history_tracks());
        assert_eq!(loaded.shuffle(), queue.shuffle());
        assert_eq!(loaded.repeat(), queue.repeat());
        // Prove the RNG survived the roundtrip by snapshotting both and
        // comparing: if the RNG differed, a resumed shuffle would diverge.
        assert_eq!(loaded.snapshot(), queue.snapshot());
    }

    #[tokio::test]
    async fn unreadable_queue_falls_back_to_empty() {
        let db = Db::open_in_memory().await.unwrap();
        db.set_setting(QUEUE_KEY, "{ not json").await.unwrap();
        let queue = db.load_queue().await.unwrap();
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn saving_an_empty_queue_keeps_playback_from_starting_on_restart() {
        let db = Db::open_in_memory().await.unwrap();
        let mut queue = sonora_core::Queue::from_tracks([1, 2]);
        queue.advance();
        queue.clear();
        db.save_queue(&queue).await.unwrap();

        let loaded = db.load_queue().await.unwrap();
        assert!(loaded.is_empty());
        assert_eq!(loaded.current(), None);
    }

    async fn seed_two_tracks(db: &Db) -> (TrackId, TrackId) {
        let folder = db.add_folder(Path::new("/music")).await.unwrap();
        let run = db.begin_scan().await.unwrap();
        let outcomes = db
            .upsert_scanned(
                folder,
                run,
                &[
                    track("/music/a.mp3", "Windowlicker", "Aphex Twin", "Windowlicker"),
                    track("/music/b.mp3", "Xtal", "Aphex Twin", "SAW"),
                ],
            )
            .await
            .unwrap();
        let id = |outcome: &UpsertOutcome| match outcome {
            UpsertOutcome::Imported(id) => *id,
            other => panic!("expected an import, got {other:?}"),
        };
        (id(&outcomes[0]), id(&outcomes[1]))
    }

    #[tokio::test]
    async fn plays_build_history_and_counts() {
        let db = Db::open_in_memory().await.unwrap();
        let (first, second) = seed_two_tracks(&db).await;

        db.record_play(first).await.unwrap();
        db.record_play(first).await.unwrap();
        db.record_play(second).await.unwrap();

        let history = db.history(10, 0).await.unwrap();
        assert_eq!(history.len(), 3);
        // Newest first, and a fresh play is not a skip.
        assert_eq!(history[0].track.id, second);
        assert!(!history[0].skipped);
        assert_eq!(history[1].track.id, first);

        let most_played = db.most_played(10).await.unwrap();
        assert_eq!(most_played[0].id, first);
        assert_eq!(most_played[0].play_count, 2);
        assert_eq!(most_played[1].id, second);

        let recently_played = db.recently_played(10).await.unwrap();
        assert_eq!(recently_played.len(), 2);
        assert!(recently_played.iter().any(|track| track.id == second));
    }

    #[tokio::test]
    async fn a_skip_marks_the_play_it_interrupted() {
        let db = Db::open_in_memory().await.unwrap();
        let (first, _) = seed_two_tracks(&db).await;

        db.record_play(first).await.unwrap();
        db.record_skip(first).await.unwrap();

        let history = db.history(10, 0).await.unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].skipped);

        let skip_count: i64 =
            sqlx::query_scalar("SELECT skip_count FROM track_activity WHERE track_id = ?1")
                .bind(first)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(skip_count, 1);
        // A skip is still a play that started.
        assert_eq!(db.track(first).await.unwrap().unwrap().play_count, 1);
    }

    #[tokio::test]
    async fn favourites_and_ratings_round_trip() {
        let db = Db::open_in_memory().await.unwrap();
        let (first, second) = seed_two_tracks(&db).await;

        db.set_favourite(first, true).await.unwrap();
        db.set_rating(first, 4).await.unwrap();

        let favourite = db.favourites(10).await.unwrap();
        assert_eq!(favourite.len(), 1);
        assert_eq!(favourite[0].id, first);
        assert!(favourite[0].favourite);
        assert_eq!(favourite[0].rating, 4);
        assert_eq!(favourite[0].play_count, 0);

        // Ratings above five are clamped instead of being rejected.
        db.set_rating(second, 99).await.unwrap();
        assert_eq!(db.track(second).await.unwrap().unwrap().rating, 5);

        // Unstarring removes the row from the favourites view.
        db.set_favourite(first, false).await.unwrap();
        assert!(db.favourites(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_a_track_removes_its_activity() {
        let db = Db::open_in_memory().await.unwrap();
        let (first, _) = seed_two_tracks(&db).await;
        db.record_play(first).await.unwrap();
        db.set_favourite(first, true).await.unwrap();

        sqlx::query("DELETE FROM tracks WHERE id = ?1")
            .bind(first)
            .execute(db.pool())
            .await
            .unwrap();

        let activity: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM track_activity")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let history: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM play_history")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(activity, 0);
        assert_eq!(history, 0);
    }
}
