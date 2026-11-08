//! SQLite persistence for the Sonora library.
//!
//! The pool is the only place that knows SQL. Callers get domain types from
//! `sonora-core` back, so the UI and scanner never depend on the schema.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use sonora_core::{
    sort_key, AlbumId, ArtistId, Error, FolderId, LibraryStats, PlaylistId, PlaylistRules, Result,
    ScanReport, ScannedTrack, Settings, TrackId, TrackView, QUEUE_KEY, SETTINGS_KEY,
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
    t.id, t.title, t.artist_id, ar.name AS artist_name, t.album_id,
    al.title AS album_title, aar.name AS album_artist_name,
    t.track_no, t.disc_no, t.year,
    t.duration_ms, t.codec, t.bitrate_kbps, t.file_size, t.added_at, t.path,
    t.replay_gain_track_db, t.replay_gain_album_db,
    t.replay_gain_track_peak, t.replay_gain_album_peak,
    COALESCE(act.favourite, 0) AS favourite,
    COALESCE(act.rating, 0) AS rating,
    COALESCE(act.play_count, 0) AS play_count
";

const ALBUM_COLUMNS: &str = "
    al.id, al.album_artist_id, al.title, aar.name AS album_artist, al.year,
    COUNT(t.id) AS track_count,
    COALESCE(SUM(t.duration_ms), 0) AS duration_ms
";

const TRACK_FROM: &str = "
    FROM tracks t
    LEFT JOIN artists ar ON ar.id = t.artist_id
    LEFT JOIN albums al ON al.id = t.album_id
    LEFT JOIN artists aar ON aar.id = al.album_artist_id
    LEFT JOIN track_activity act ON act.track_id = t.id
";

/// The filter shared by a smart playlist's count and its tracks.
///
/// Every value arrives as a bound parameter: the listener's text never becomes
/// SQL. `?1` is the escaped LIKE pattern, `?2`..`?5` the rating, the favourites
/// flag and the year bounds, where zero and `NULL` mean "no constraint".
const SMART_WHERE: &str = "
    WHERE (?1 = '' OR t.title LIKE ?1 ESCAPE '\\'
           OR ar.name LIKE ?1 ESCAPE '\\'
           OR al.title LIKE ?1 ESCAPE '\\')
      AND (?2 = 0 OR COALESCE(act.rating, 0) >= ?2)
      AND (?3 = 0 OR COALESCE(act.favourite, 0) = 1)
      AND (?4 IS NULL OR t.year >= ?4)
      AND (?5 IS NULL OR t.year <= ?5)
";

/// Wrap a listener's filter in LIKE wildcards, escaping the wildcards it may
/// have typed so `100%` matches a title and not every title.
fn like_pattern(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len() + 2);
    pattern.push('%');
    for ch in text.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

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
        let sql = format!(
            "SELECT {ALBUM_COLUMNS}
             FROM albums al
             JOIN artists aar ON aar.id = al.album_artist_id
             LEFT JOIN tracks t ON t.album_id = al.id
             GROUP BY al.id
             ORDER BY aar.sort_name, al.year, al.title"
        );
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await.db()?;
        Ok(rows.iter().map(map_album).collect())
    }

    /// One album on its own, for a page that knows the id but not the details.
    pub async fn album(&self, album: AlbumId) -> Result<Option<AlbumSummary>> {
        let sql = format!(
            "SELECT {ALBUM_COLUMNS}
             FROM albums al
             JOIN artists aar ON aar.id = al.album_artist_id
             LEFT JOIN tracks t ON t.album_id = al.id
             WHERE al.id = ?1
             GROUP BY al.id"
        );
        let row = sqlx::query(&sql)
            .bind(album)
            .fetch_optional(&self.pool)
            .await
            .db()?;
        Ok(row.as_ref().map(map_album))
    }

    /// Albums this artist is credited for, earliest release first.
    ///
    /// Album-artist credits are what make a release "by" someone, so an artist
    /// page built on this lists their own records and not the compilations they
    /// only appear on.
    pub async fn artist_albums(&self, artist: ArtistId) -> Result<Vec<AlbumSummary>> {
        let sql = format!(
            "SELECT {ALBUM_COLUMNS}
             FROM albums al
             JOIN artists aar ON aar.id = al.album_artist_id
             LEFT JOIN tracks t ON t.album_id = al.id
             WHERE al.album_artist_id = ?1
             GROUP BY al.id
             ORDER BY COALESCE(al.year, 0), al.title"
        );
        let rows = sqlx::query(&sql)
            .bind(artist)
            .fetch_all(&self.pool)
            .await
            .db()?;
        Ok(rows.iter().map(map_album).collect())
    }

    /// Everything this artist performs, grouped by release and in running order.
    pub async fn artist_tracks(&self, artist: ArtistId) -> Result<Vec<TrackView>> {
        let sql = format!(
            "SELECT {TRACK_COLUMNS} {TRACK_FROM}
             WHERE t.artist_id = ?1
             ORDER BY COALESCE(al.year, 0), al.title,
                      COALESCE(t.disc_no, 1), COALESCE(t.track_no, 9999), t.title"
        );
        let rows = sqlx::query(&sql)
            .bind(artist)
            .fetch_all(&self.pool)
            .await
            .db()?;
        Ok(rows.iter().map(map_track).collect())
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

    // --------------------------------------------------------------- playlists

    /// Every playlist, alphabetically, with how many tracks each holds right now.
    ///
    /// A smart playlist's count is whatever its rules currently select, so the
    /// number on the Playlists page follows the library without a rescan of the
    /// playlist itself.
    pub async fn playlists(&self) -> Result<Vec<PlaylistSummary>> {
        let rows = sqlx::query(
            "SELECT p.id, p.name, p.rules
             FROM playlists p
             ORDER BY p.name COLLATE NOCASE, p.id",
        )
        .fetch_all(&self.pool)
        .await
        .db()?;
        // One grouped query for the hand-built lists beats a count per playlist.
        let counts = sqlx::query(
            "SELECT playlist_id, COUNT(*) AS entries FROM playlist_tracks GROUP BY playlist_id",
        )
        .fetch_all(&self.pool)
        .await
        .db()?;
        let entries: HashMap<PlaylistId, i64> = counts
            .iter()
            .map(|row| (row.get("playlist_id"), row.get("entries")))
            .collect();

        let mut summaries = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: PlaylistId = row.get("id");
            let rules: Option<String> = row.get("rules");
            let track_count = match &rules {
                Some(json) => {
                    self.smart_playlist_count(&PlaylistRules::from_json(json)?)
                        .await?
                }
                None => entries.get(&id).copied().unwrap_or(0),
            };
            summaries.push(PlaylistSummary {
                id,
                name: row.get("name"),
                track_count,
                smart: rules.is_some(),
            });
        }
        Ok(summaries)
    }

    pub async fn playlist(&self, id: PlaylistId) -> Result<Option<PlaylistSummary>> {
        let row = sqlx::query("SELECT id, name, rules FROM playlists WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .db()?;
        let Some(row) = row else {
            return Ok(None);
        };
        let rules: Option<String> = row.get("rules");
        let track_count = match &rules {
            Some(json) => {
                self.smart_playlist_count(&PlaylistRules::from_json(json)?)
                    .await?
            }
            None => {
                let entries: i64 =
                    sqlx::query("SELECT COUNT(*) FROM playlist_tracks WHERE playlist_id = ?1")
                        .bind(id)
                        .fetch_one(&self.pool)
                        .await
                        .db()?
                        .get(0);
                entries
            }
        };
        Ok(Some(PlaylistSummary {
            id: row.get("id"),
            name: row.get("name"),
            track_count,
            smart: rules.is_some(),
        }))
    }

    /// The rules behind a smart playlist, or `None` for a hand-built playlist or
    /// one that no longer exists.
    pub async fn playlist_rules(&self, id: PlaylistId) -> Result<Option<PlaylistRules>> {
        let row = sqlx::query("SELECT rules FROM playlists WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .db()?;
        match row.and_then(|row| row.get::<Option<String>, _>("rules")) {
            Some(json) => Ok(Some(PlaylistRules::from_json(&json)?)),
            None => Ok(None),
        }
    }

    /// Make a playlist that fills itself from `rules`.
    pub async fn create_smart_playlist(
        &self,
        name: &str,
        rules: &PlaylistRules,
    ) -> Result<PlaylistId> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Other("a playlist needs a name".into()));
        }
        let result =
            sqlx::query("INSERT INTO playlists (name, created_at, rules) VALUES (?1, ?2, ?3)")
                .bind(name)
                .bind(now())
                .bind(rules.to_json()?)
                .execute(&self.pool)
                .await
                .db()?;
        Ok(result.last_insert_rowid())
    }

    /// Replace a smart playlist's rules. Hand-built playlists are refused: making
    /// one smart would throw away the order the listener arranged.
    pub async fn set_playlist_rules(&self, id: PlaylistId, rules: &PlaylistRules) -> Result<()> {
        let result =
            sqlx::query("UPDATE playlists SET rules = ?2 WHERE id = ?1 AND rules IS NOT NULL")
                .bind(id)
                .bind(rules.to_json()?)
                .execute(&self.pool)
                .await
                .db()?;
        if result.rows_affected() == 0 {
            return Err(Error::Other(format!("no smart playlist {id}")));
        }
        Ok(())
    }

    /// The tracks `rules` currently select, in album order so a limited playlist
    /// still reads like a shelf rather than a random sample.
    pub async fn smart_playlist_tracks(&self, rules: &PlaylistRules) -> Result<Vec<TrackView>> {
        let rules = rules.clone().normalized();
        let sql = format!(
            "SELECT {TRACK_COLUMNS} {TRACK_FROM} {SMART_WHERE}
             ORDER BY COALESCE(aar.sort_name, ''), COALESCE(al.title, ''),
                      COALESCE(t.disc_no, 1), COALESCE(t.track_no, 9999), t.title
             LIMIT ?6"
        );
        let rows = sqlx::query(&sql)
            .bind(like_pattern(&rules.text))
            .bind(i64::from(rules.min_rating))
            .bind(i64::from(rules.favourites_only))
            .bind(rules.year_from)
            .bind(rules.year_to)
            .bind(rules.limit)
            .fetch_all(&self.pool)
            .await
            .db()?;
        Ok(rows.iter().map(map_track).collect())
    }

    async fn smart_playlist_count(&self, rules: &PlaylistRules) -> Result<i64> {
        let rules = rules.clone().normalized();
        let sql = format!("SELECT COUNT(*) {TRACK_FROM} {SMART_WHERE}");
        let count: i64 = sqlx::query(&sql)
            .bind(like_pattern(&rules.text))
            .bind(i64::from(rules.min_rating))
            .bind(i64::from(rules.favourites_only))
            .bind(rules.year_from)
            .bind(rules.year_to)
            .fetch_one(&self.pool)
            .await
            .db()?
            .get(0);
        Ok(count)
    }

    /// Make a playlist with `name`. Surrounding space is dropped, and a name that
    /// is only space is refused rather than stored as a playlist nobody can find.
    pub async fn create_playlist(&self, name: &str) -> Result<PlaylistId> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Other("a playlist needs a name".into()));
        }
        let result = sqlx::query("INSERT INTO playlists (name, created_at) VALUES (?1, ?2)")
            .bind(name)
            .bind(now())
            .execute(&self.pool)
            .await
            .db()?;
        Ok(result.last_insert_rowid())
    }

    pub async fn rename_playlist(&self, id: PlaylistId, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Other("a playlist needs a name".into()));
        }
        let result = sqlx::query("UPDATE playlists SET name = ?2 WHERE id = ?1")
            .bind(id)
            .bind(name)
            .execute(&self.pool)
            .await
            .db()?;
        if result.rows_affected() == 0 {
            return Err(Error::Other(format!("no playlist {id}")));
        }
        Ok(())
    }

    /// Remove a playlist; its entries go with it, the tracks themselves stay.
    pub async fn delete_playlist(&self, id: PlaylistId) -> Result<()> {
        sqlx::query("DELETE FROM playlists WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .db()?;
        Ok(())
    }

    /// A playlist's tracks: the order the listener arranged for a hand-built
    /// list, or whatever the rules select for a smart one.
    pub async fn playlist_tracks(&self, id: PlaylistId) -> Result<Vec<TrackView>> {
        if let Some(rules) = self.playlist_rules(id).await? {
            return self.smart_playlist_tracks(&rules).await;
        }
        let sql = format!(
            "SELECT {TRACK_COLUMNS} {TRACK_FROM}
             WHERE t.id IN (SELECT track_id FROM playlist_tracks WHERE playlist_id = ?1)
             ORDER BY (SELECT position FROM playlist_tracks pt
                       WHERE pt.playlist_id = ?1 AND pt.track_id = t.id), t.title"
        );
        let rows = sqlx::query(&sql)
            .bind(id)
            .fetch_all(&self.pool)
            .await
            .db()?;
        Ok(rows.iter().map(map_track).collect())
    }

    /// Append a track. Adding the same track twice is a no-op, so the answer says
    /// whether the playlist actually grew.
    pub async fn add_to_playlist(&self, id: PlaylistId, track: TrackId) -> Result<bool> {
        self.ensure_hand_built(
            id,
            "add a track to a smart playlist; change its rules instead",
        )
        .await?;
        let next: i64 = sqlx::query(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM playlist_tracks WHERE playlist_id = ?1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .db()?
        .get::<i64, _>(0);
        let result = sqlx::query(
            "INSERT INTO playlist_tracks (playlist_id, track_id, position, added_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (playlist_id, track_id) DO NOTHING",
        )
        .bind(id)
        .bind(track)
        .bind(next)
        .bind(now())
        .execute(&self.pool)
        .await
        .db()?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn remove_from_playlist(&self, id: PlaylistId, track: TrackId) -> Result<bool> {
        self.ensure_hand_built(
            id,
            "remove a track from a smart playlist; change its rules instead",
        )
        .await?;
        let removed =
            sqlx::query("DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND track_id = ?2")
                .bind(id)
                .bind(track)
                .execute(&self.pool)
                .await
                .db()?
                .rows_affected()
                > 0;
        if removed {
            // Close the gap so positions stay dense and the next move can be
            // expressed as a plain index.
            let order = self.playlist_order(id).await?;
            self.write_playlist_order(id, &order).await?;
        }
        Ok(removed)
    }

    /// Move the entry at `from` so it sits at `to`, shifting the entries between.
    pub async fn move_playlist_track(&self, id: PlaylistId, from: i64, to: i64) -> Result<()> {
        self.ensure_hand_built(id, "reorder a smart playlist; it has no order to arrange")
            .await?;
        let mut order = self.playlist_order(id).await?;
        let len = order.len() as i64;
        if !(0..len).contains(&from) || !(0..len).contains(&to) {
            return Err(Error::Other(format!(
                "playlist {id} has no entry {}",
                if (0..len).contains(&from) { to } else { from }
            )));
        }
        if from == to {
            return Ok(());
        }
        let moved = order.remove(from as usize);
        order.insert(to as usize, moved);
        self.write_playlist_order(id, &order).await
    }

    /// Refuse a hand edit on a playlist that fills itself, and say why. A smart
    /// playlist's entries are derived, so an insert or a move would either be
    /// discarded on the next read or fight the rules.
    async fn ensure_hand_built(&self, id: PlaylistId, action: &str) -> Result<()> {
        let row = sqlx::query("SELECT rules IS NOT NULL AS smart FROM playlists WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .db()?;
        let Some(row) = row else {
            return Err(Error::Other(format!("no playlist {id}")));
        };
        if row.get::<i64, _>("smart") != 0 {
            return Err(Error::Other(format!("cannot {action}")));
        }
        Ok(())
    }

    async fn playlist_order(&self, id: PlaylistId) -> Result<Vec<TrackId>> {
        let rows = sqlx::query(
            "SELECT track_id FROM playlist_tracks WHERE playlist_id = ?1
             ORDER BY position, added_at",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .db()?;
        Ok(rows.iter().map(|row| row.get("track_id")).collect())
    }

    async fn write_playlist_order(&self, id: PlaylistId, order: &[TrackId]) -> Result<()> {
        let mut tx = self.pool.begin().await.db()?;
        for (position, track) in order.iter().enumerate() {
            sqlx::query(
                "UPDATE playlist_tracks SET position = ?3
                 WHERE playlist_id = ?1 AND track_id = ?2",
            )
            .bind(id)
            .bind(track)
            .bind(position as i64)
            .execute(&mut *tx)
            .await
            .db()?;
        }
        tx.commit().await.db()
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
    /// The artist credited for the release as a whole, so a name on an album
    /// page can open everything by that artist.
    pub album_artist_id: ArtistId,
    pub title: String,
    pub album_artist: String,
    pub year: Option<i64>,
    pub track_count: i64,
    pub duration_ms: i64,
}

/// One row on the Playlists page: a name and how much is in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistSummary {
    pub id: PlaylistId,
    pub name: String,
    pub track_count: i64,
    /// Whether the tracks come from rules rather than from the listener's order.
    pub smart: bool,
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

fn map_album(row: &SqliteRow) -> AlbumSummary {
    AlbumSummary {
        id: row.get("id"),
        album_artist_id: row.get("album_artist_id"),
        title: row.get("title"),
        album_artist: row.get("album_artist"),
        year: row.try_get::<Option<i64>, _>("year").unwrap_or(None),
        track_count: row.get("track_count"),
        duration_ms: row.get("duration_ms"),
    }
}

fn map_track(row: &SqliteRow) -> TrackView {
    TrackView {
        id: row.get("id"),
        title: row.get("title"),
        artist: opt_string(row, "artist_name"),
        artist_id: row.try_get::<Option<i64>, _>("artist_id").unwrap_or(None),
        album: opt_string(row, "album_title"),
        album_id: row.try_get::<Option<i64>, _>("album_id").unwrap_or(None),
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
    async fn artist_pages_list_their_albums_and_tracks() {
        let db = Db::open_in_memory().await.unwrap();
        let folder = db.add_folder(Path::new("/music")).await.unwrap();
        let run = db.begin_scan().await.unwrap();

        let mut first = track("/music/1.mp3", "Xtal", "Aphex Twin", "SAW 85-92");
        first.year = Some(1992);
        first.track_no = Some(1);
        let mut second = track("/music/2.mp3", "Tha", "Aphex Twin", "SAW 85-92");
        second.year = Some(1992);
        second.track_no = Some(2);
        let mut later = track("/music/8.mp3", "Windowlicker", "Aphex Twin", "Windowlicker");
        later.year = Some(1999);
        let other = track(
            "/music/9.mp3",
            "Roygbiv",
            "Boards of Canada",
            "Music Has the Right",
        );

        db.upsert_scanned(folder, run, &[later, second, first, other])
            .await
            .unwrap();

        let albums = db.albums().await.unwrap();
        let saw = albums
            .iter()
            .find(|album| album.title == "SAW 85-92")
            .expect("album");
        let boards = albums
            .iter()
            .find(|album| album.title == "Music Has the Right")
            .expect("album");
        assert_ne!(saw.album_artist_id, boards.album_artist_id);

        // A single album answers with the same counts the list shows.
        let single = db.album(saw.id).await.unwrap().expect("album");
        assert_eq!(single.track_count, 2);
        assert_eq!(db.album(9_999).await.unwrap(), None);

        let titles: Vec<String> = db
            .artist_albums(saw.album_artist_id)
            .await
            .unwrap()
            .into_iter()
            .map(|album| album.title)
            .collect();
        assert_eq!(titles, vec!["SAW 85-92", "Windowlicker"]);

        let tracks: Vec<String> = db
            .artist_tracks(saw.album_artist_id)
            .await
            .unwrap()
            .into_iter()
            .map(|track| track.title)
            .collect();
        assert_eq!(tracks, vec!["Xtal", "Tha", "Windowlicker"]);

        // Another artist's page must not pick up Aphex Twin's release.
        let boards_tracks = db.artist_tracks(boards.album_artist_id).await.unwrap();
        assert_eq!(boards_tracks.len(), 1);
        assert_eq!(boards_tracks[0].title, "Roygbiv");
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
    async fn playlists_keep_their_own_order() {
        let db = Db::open_in_memory().await.unwrap();
        let (first, second) = seed_two_tracks(&db).await;

        let morning = db.create_playlist("  Morning  ").await.unwrap();
        let night = db.create_playlist("Night").await.unwrap();
        assert_eq!(db.playlist(morning).await.unwrap().unwrap().name, "Morning");

        // Appended in the order added, and adding one twice changes nothing.
        assert!(db.add_to_playlist(morning, second).await.unwrap());
        assert!(db.add_to_playlist(morning, first).await.unwrap());
        assert!(!db.add_to_playlist(morning, first).await.unwrap());
        assert!(db.add_to_playlist(night, first).await.unwrap());

        let order: Vec<TrackId> = db
            .playlist_tracks(morning)
            .await
            .unwrap()
            .iter()
            .map(|track| track.id)
            .collect();
        assert_eq!(order, vec![second, first]);

        // Each playlist counts only its own tracks.
        let summaries = db.playlists().await.unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].name, "Morning");
        assert_eq!(summaries[0].track_count, 2);
        assert_eq!(summaries[1].track_count, 1);

        // Removing the middle entry closes the gap it leaves behind.
        assert!(db.remove_from_playlist(morning, second).await.unwrap());
        assert!(!db.remove_from_playlist(morning, second).await.unwrap());
        assert_eq!(db.playlist_tracks(morning).await.unwrap().len(), 1);
        assert!(db.add_to_playlist(morning, second).await.unwrap());
        let order: Vec<TrackId> = db
            .playlist_tracks(morning)
            .await
            .unwrap()
            .iter()
            .map(|track| track.id)
            .collect();
        assert_eq!(order, vec![first, second]);
    }

    #[tokio::test]
    async fn playlist_tracks_move_and_refuse_bad_indexes() {
        let db = Db::open_in_memory().await.unwrap();
        let (first, second) = seed_two_tracks(&db).await;
        let folder = db.add_folder(Path::new("/more")).await.unwrap();
        let run = db.begin_scan().await.unwrap();
        let third = match db
            .upsert_scanned(
                folder,
                run,
                &[track(
                    "/more/c.mp3",
                    "Roygbiv",
                    "Boards of Canada",
                    "MHTRTC",
                )],
            )
            .await
            .unwrap()[0]
        {
            UpsertOutcome::Imported(id) => id,
            other => panic!("expected an import, got {other:?}"),
        };

        let list = db.create_playlist("Mix").await.unwrap();
        for id in [first, second, third] {
            db.add_to_playlist(list, id).await.unwrap();
        }

        db.move_playlist_track(list, 2, 0).await.unwrap();
        let order: Vec<TrackId> = db
            .playlist_tracks(list)
            .await
            .unwrap()
            .iter()
            .map(|track| track.id)
            .collect();
        assert_eq!(order, vec![third, first, second]);

        db.move_playlist_track(list, 0, 2).await.unwrap();
        let order: Vec<TrackId> = db
            .playlist_tracks(list)
            .await
            .unwrap()
            .iter()
            .map(|track| track.id)
            .collect();
        assert_eq!(order, vec![first, second, third]);

        // Out-of-range moves are refused instead of silently reordering.
        assert!(db.move_playlist_track(list, 0, 3).await.is_err());
        assert!(db.move_playlist_track(list, -1, 0).await.is_err());
    }

    #[tokio::test]
    async fn deleting_a_playlist_keeps_the_tracks() {
        let db = Db::open_in_memory().await.unwrap();
        let (first, _) = seed_two_tracks(&db).await;
        let list = db.create_playlist("Temporary").await.unwrap();
        db.add_to_playlist(list, first).await.unwrap();

        db.delete_playlist(list).await.unwrap();

        assert!(db.playlist(list).await.unwrap().is_none());
        assert!(db.playlists().await.unwrap().is_empty());
        assert!(db.playlist_tracks(list).await.unwrap().is_empty());
        assert_eq!(db.stats().await.unwrap().tracks, 2);
    }

    #[tokio::test]
    async fn renaming_needs_a_real_name() {
        let db = Db::open_in_memory().await.unwrap();
        let list = db.create_playlist("Draft").await.unwrap();

        db.rename_playlist(list, " Finished ").await.unwrap();
        assert_eq!(db.playlist(list).await.unwrap().unwrap().name, "Finished");

        assert!(db.create_playlist("   ").await.is_err());
        assert!(db.rename_playlist(list, "").await.is_err());
        assert_eq!(db.playlist(list).await.unwrap().unwrap().name, "Finished");
    }

    #[tokio::test]
    async fn smart_playlists_follow_the_library() {
        let db = Db::open_in_memory().await.unwrap();
        let (first, second) = seed_two_tracks(&db).await;

        let starred = db
            .create_smart_playlist(
                "Starred",
                &PlaylistRules {
                    min_rating: 4,
                    ..PlaylistRules::default()
                },
            )
            .await
            .unwrap();
        // Empty rules select the whole library, so an unfiltered smart playlist is
        // usable before anyone edits it.
        let everything = db
            .create_smart_playlist("Everything", &PlaylistRules::default())
            .await
            .unwrap();
        assert_eq!(db.playlist_tracks(everything).await.unwrap().len(), 2);

        // Editing the library is the only way a smart playlist changes.
        assert!(db.playlist_tracks(starred).await.unwrap().is_empty());
        db.set_rating(first, 5).await.unwrap();
        let picked: Vec<TrackId> = db
            .playlist_tracks(starred)
            .await
            .unwrap()
            .iter()
            .map(|track| track.id)
            .collect();
        assert_eq!(picked, vec![first]);

        db.set_rating(second, 4).await.unwrap();
        let count = db.playlist(starred).await.unwrap().unwrap().track_count;
        assert_eq!(count, 2, "the summary counts what the rules select now");

        // Dropping a rating drops the track again, with no playlist edit at all.
        db.set_rating(first, 1).await.unwrap();
        let picked: Vec<TrackId> = db
            .playlist_tracks(starred)
            .await
            .unwrap()
            .iter()
            .map(|track| track.id)
            .collect();
        assert_eq!(picked, vec![second]);

        let summaries = db.playlists().await.unwrap();
        assert_eq!(summaries.len(), 2);
        assert!(summaries.iter().all(|summary| summary.smart));
        assert_eq!(summaries[0].name, "Everything");
        assert_eq!(summaries[0].track_count, 2);
        assert_eq!(summaries[1].track_count, 1);
    }

    #[tokio::test]
    async fn smart_rules_filter_by_text_favourite_year_and_limit() {
        let db = Db::open_in_memory().await.unwrap();
        let (first, second) = seed_two_tracks(&db).await;
        let folder = db.add_folder(Path::new("/more")).await.unwrap();
        let run = db.begin_scan().await.unwrap();
        let third = match db
            .upsert_scanned(
                folder,
                run,
                &[track(
                    "/more/c.mp3",
                    "Roygbiv",
                    "Boards of Canada",
                    "MHTRTC",
                )],
            )
            .await
            .unwrap()[0]
        {
            UpsertOutcome::Imported(id) => id,
            other => panic!("expected an import, got {other:?}"),
        };
        db.set_favourite(second, true).await.unwrap();

        let ids = |tracks: Vec<sonora_core::TrackView>| -> Vec<TrackId> {
            tracks.iter().map(|track| track.id).collect()
        };

        // Text matches an artist, an album or a title, case-insensitively.
        let by_artist = PlaylistRules {
            text: "boards".into(),
            ..PlaylistRules::default()
        };
        assert_eq!(
            ids(db.smart_playlist_tracks(&by_artist).await.unwrap()),
            vec![third]
        );
        let by_album = PlaylistRules {
            text: "saw".into(),
            ..PlaylistRules::default()
        };
        assert_eq!(
            ids(db.smart_playlist_tracks(&by_album).await.unwrap()),
            vec![second]
        );
        let by_title = PlaylistRules {
            text: "WINDOW".into(),
            ..PlaylistRules::default()
        };
        assert_eq!(
            ids(db.smart_playlist_tracks(&by_title).await.unwrap()),
            vec![first]
        );

        // A typed wildcard is matched literally rather than matching everything.
        let literal = PlaylistRules {
            text: "%".into(),
            ..PlaylistRules::default()
        };
        assert!(db.smart_playlist_tracks(&literal).await.unwrap().is_empty());

        let favourites = PlaylistRules {
            favourites_only: true,
            ..PlaylistRules::default()
        };
        assert_eq!(
            ids(db.smart_playlist_tracks(&favourites).await.unwrap()),
            vec![second]
        );

        // The fixtures are all from 1999.
        let decades = PlaylistRules {
            year_from: Some(1990),
            year_to: Some(1999),
            ..PlaylistRules::default()
        };
        assert_eq!(db.smart_playlist_tracks(&decades).await.unwrap().len(), 3);
        let later = PlaylistRules {
            year_from: Some(2005),
            ..PlaylistRules::default()
        };
        assert!(db.smart_playlist_tracks(&later).await.unwrap().is_empty());

        // A limit caps the answer, and a nonsense one is clamped into range.
        let capped = PlaylistRules {
            limit: 2,
            ..PlaylistRules::default()
        };
        assert_eq!(db.smart_playlist_tracks(&capped).await.unwrap().len(), 2);
        let zero = PlaylistRules {
            limit: 0,
            ..PlaylistRules::default()
        };
        assert_eq!(db.smart_playlist_tracks(&zero).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn smart_playlists_refuse_hand_edits_and_keep_their_rules() {
        let db = Db::open_in_memory().await.unwrap();
        let (first, _) = seed_two_tracks(&db).await;
        let rules = PlaylistRules {
            text: "aphex".into(),
            min_rating: 0,
            favourites_only: false,
            year_from: Some(1990),
            year_to: None,
            limit: 50,
        };
        let smart = db.create_smart_playlist("Smart", &rules).await.unwrap();
        let manual = db.create_playlist("Manual").await.unwrap();

        // The rules round-trip through the database unchanged.
        assert_eq!(db.playlist_rules(smart).await.unwrap(), Some(rules.clone()));
        assert_eq!(db.playlist_rules(manual).await.unwrap(), None);

        // A hand edit on a smart playlist is refused, and the rules still win.
        assert!(db.add_to_playlist(smart, first).await.is_err());
        assert!(db.remove_from_playlist(smart, first).await.is_err());
        assert!(db.move_playlist_track(smart, 0, 0).await.is_err());
        assert_eq!(db.playlist_tracks(smart).await.unwrap().len(), 2);

        // Editing the rules replaces them, normalized.
        let stashed = db
            .set_playlist_rules(
                smart,
                &PlaylistRules {
                    min_rating: 9,
                    ..PlaylistRules::default()
                },
            )
            .await;
        assert!(stashed.is_ok());
        let stored = db.playlist_rules(smart).await.unwrap().unwrap();
        assert_eq!(stored.min_rating, 5);
        assert!(stored.text.is_empty());
        assert_eq!(db.playlist_tracks(smart).await.unwrap().len(), 0);

        // A hand-built playlist cannot be turned into a smart one behind the
        // listener's back, and unknown ids are refused rather than ignored.
        assert!(db.set_playlist_rules(manual, &rules).await.is_err());
        assert!(db.set_playlist_rules(9_999, &rules).await.is_err());
        assert!(db.create_smart_playlist("  ", &rules).await.is_err());

        // Renaming a smart playlist keeps it smart.
        db.rename_playlist(smart, "Renamed").await.unwrap();
        let summary = db.playlist(smart).await.unwrap().unwrap();
        assert_eq!(summary.name, "Renamed");
        assert!(summary.smart);
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
