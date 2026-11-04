//! Recursive library scanning.
//!
//! The walker runs on a blocking thread (tag parsing is CPU and IO bound) and
//! streams batches of tracks to the async side, which writes them in
//! transactions. That keeps memory flat on 100k-track libraries and lets the UI
//! receive progress while the scan is still running.

pub mod metadata;

use std::path::{Path, PathBuf};
use std::time::Instant;

use sonora_core::{
    is_supported_audio, Error, Event, Result, ScanError, ScanProgress, ScanReport, ScannedTrack,
};
use sonora_db::{Db, UpsertOutcome};
use tokio::sync::{broadcast, mpsc};
use walkdir::{DirEntry, WalkDir};

const BATCH_SIZE: usize = 128;

#[derive(Clone)]
pub struct Scanner {
    db: Db,
}

enum ScanBatch {
    Tracks(Vec<ScannedTrack>),
    Errors(Vec<ScanError>),
}

impl Scanner {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Index every supported audio file under `folder`.
    ///
    /// The folder is registered as a library folder, files are inserted or
    /// updated, and rows for files that vanished are removed once the walk
    /// completes. A second scan of an unchanged library writes nothing.
    pub async fn scan(
        &self,
        folder: impl AsRef<Path>,
        events: Option<broadcast::Sender<Event>>,
    ) -> Result<ScanReport> {
        let started = Instant::now();
        let folder = canonical_dir(folder.as_ref())?;
        let folder_id = self.db.add_folder(&folder).await?;
        let run_id = self.db.begin_scan().await?;

        if let Some(tx) = events.as_ref() {
            let _ = tx.send(Event::ScanStarted {
                folder: folder.clone(),
            });
        }

        let (batch_tx, mut batch_rx) = mpsc::channel::<ScanBatch>(4);
        let root = folder.clone();
        let walker = tokio::task::spawn_blocking(move || walk(root, batch_tx));

        let mut report = ScanReport {
            folder: folder.clone(),
            ..Default::default()
        };
        let mut last_progress = Instant::now();

        while let Some(batch) = batch_rx.recv().await {
            match batch {
                ScanBatch::Tracks(tracks) => {
                    report.seen += tracks.len() as u64;
                    match self.db.upsert_scanned(folder_id, run_id, &tracks).await {
                        Ok(outcomes) => {
                            for outcome in outcomes {
                                match outcome {
                                    UpsertOutcome::Imported(_) => report.imported += 1,
                                    UpsertOutcome::Updated(_) => report.updated += 1,
                                    UpsertOutcome::Unchanged(_) => report.unchanged += 1,
                                }
                            }
                        }
                        Err(err) => report.errors.push(ScanError {
                            path: tracks
                                .first()
                                .map(|track| track.path.clone())
                                .unwrap_or_default(),
                            message: err.to_string(),
                        }),
                    }
                }
                ScanBatch::Errors(errors) => report.errors.extend(errors),
            }

            if last_progress.elapsed().as_millis() >= 150 {
                emit_progress(&events, &report);
                last_progress = Instant::now();
            }
        }

        walker
            .await
            .map_err(|err| Error::Other(format!("scanner task failed: {err}")))?;

        // Anything in this folder not touched by this run is gone.
        report.removed = self.db.prune_missing(folder_id, run_id).await?;
        self.db.finish_scan(run_id, folder_id, &report).await?;
        report.elapsed_ms = started.elapsed().as_millis();

        if let Some(tx) = events.as_ref() {
            emit_progress(&events, &report);
            let _ = tx.send(Event::ScanFinished(report.clone()));
            if let Ok(stats) = self.db.stats().await {
                let _ = tx.send(Event::LibraryChanged(stats));
            }
        }

        log::info!(
            "scan of {} finished: seen={} imported={} updated={} unchanged={} removed={} errors={} in {}ms",
            folder.display(),
            report.seen,
            report.imported,
            report.updated,
            report.unchanged,
            report.removed,
            report.total_errors(),
            report.elapsed_ms
        );

        Ok(report)
    }

    /// Re-scan every registered library folder.
    pub async fn scan_all(
        &self,
        events: Option<broadcast::Sender<Event>>,
    ) -> Result<Vec<ScanReport>> {
        let mut reports = Vec::new();
        for (_, folder) in self.db.folders().await? {
            reports.push(self.scan(folder, events.clone()).await?);
        }
        Ok(reports)
    }
}

fn emit_progress(events: &Option<broadcast::Sender<Event>>, report: &ScanReport) {
    if let Some(tx) = events.as_ref() {
        let _ = tx.send(Event::ScanProgress(ScanProgress {
            folder: report.folder.clone(),
            seen: report.seen,
            imported: report.imported,
            updated: report.updated,
            unchanged: report.unchanged,
            errors: report.total_errors(),
            current: None,
        }));
    }
}

fn canonical_dir(path: &Path) -> Result<PathBuf> {
    let canonical = path.canonicalize().map_err(|err| Error::io(path, err))?;
    if !canonical.is_dir() {
        return Err(Error::Config(format!(
            "{} is not a folder",
            canonical.display()
        )));
    }
    Ok(canonical)
}

/// Blocking walk. Sends batches until the receiving side hangs up.
fn walk(root: PathBuf, tx: mpsc::Sender<ScanBatch>) {
    let mut tracks: Vec<ScannedTrack> = Vec::with_capacity(BATCH_SIZE);
    let mut errors: Vec<ScanError> = Vec::new();

    let walker = WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(is_relevant_entry);

    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                errors.push(ScanError {
                    path: err
                        .path()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| root.clone()),
                    message: err.to_string(),
                });
                if errors.len() >= 32
                    && tx
                        .blocking_send(ScanBatch::Errors(std::mem::take(&mut errors)))
                        .is_err()
                {
                    return;
                }
                continue;
            }
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        if !is_supported_audio(path) {
            continue;
        }

        match metadata::extract(path) {
            Ok(track) => tracks.push(track),
            Err(err) => {
                log::warn!("skipping {}: {err}", path.display());
                errors.push(ScanError {
                    path: path.to_path_buf(),
                    message: err.to_string(),
                });
            }
        }

        if tracks.len() >= BATCH_SIZE
            && tx
                .blocking_send(ScanBatch::Tracks(std::mem::take(&mut tracks)))
                .is_err()
        {
            return;
        }
        if errors.len() >= 32
            && tx
                .blocking_send(ScanBatch::Errors(std::mem::take(&mut errors)))
                .is_err()
        {
            return;
        }
    }

    if !tracks.is_empty() {
        let _ = tx.blocking_send(ScanBatch::Tracks(tracks));
    }
    if !errors.is_empty() {
        let _ = tx.blocking_send(ScanBatch::Errors(errors));
    }
}

/// Skip hidden directories and the junk that removable media brings with it.
fn is_relevant_entry(entry: &DirEntry) -> bool {
    // `filter_entry` also visits the root itself — never filter that out, or a
    // library folder whose own name starts with a dot would scan as empty.
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    if name.starts_with('.') {
        return false;
    }
    !matches!(
        name.as_ref(),
        "$RECYCLE.BIN" | "System Volume Information" | "__MACOSX" | "node_modules"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal 1-second 8 kHz mono PCM16 WAV file.
    fn write_wav(path: &Path, seconds: u32) {
        let sample_rate: u32 = 8_000;
        let channels: u16 = 1;
        let bits: u16 = 16;
        let samples = sample_rate * seconds;
        let data_len = samples * channels as u32 * (bits / 8) as u32;
        let byte_rate = sample_rate * channels as u32 * (bits / 8) as u32;

        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt ").unwrap();
        file.write_all(&16u32.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        file.write_all(&channels.to_le_bytes()).unwrap();
        file.write_all(&sample_rate.to_le_bytes()).unwrap();
        file.write_all(&byte_rate.to_le_bytes()).unwrap();
        file.write_all(&(channels * bits / 8).to_le_bytes())
            .unwrap();
        file.write_all(&bits.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&data_len.to_le_bytes()).unwrap();
        file.write_all(&vec![0u8; data_len as usize]).unwrap();
    }

    #[tokio::test]
    async fn scans_a_folder_and_is_incremental() {
        let dir = tempfile::tempdir().unwrap();
        write_wav(&dir.path().join("Artist - First.wav"), 1);
        write_wav(&dir.path().join("02 - Second.wav"), 2);
        std::fs::write(dir.path().join("notes.txt"), b"not audio").unwrap();

        let db = Db::open_in_memory().await.unwrap();
        let scanner = Scanner::new(db.clone());

        let report = scanner.scan(dir.path(), None).await.unwrap();
        assert_eq!(report.imported, 2);
        assert_eq!(report.seen, 2);
        assert_eq!(report.total_errors(), 0);

        let stats = db.stats().await.unwrap();
        assert_eq!(stats.tracks, 2);
        assert_eq!(stats.artists, 1, "the second file has no artist tag");

        let tracks = db.list_tracks(10, 0).await.unwrap();
        let first = tracks
            .iter()
            .find(|track| track.title == "First")
            .expect("tagless file name fallback produced a title");
        assert_eq!(first.artist.as_deref(), Some("Artist"));
        assert_eq!(first.duration_ms, 1000);

        // A second pass must not reprocess anything.
        let report = scanner.scan(dir.path(), None).await.unwrap();
        assert_eq!(report.imported, 0);
        assert_eq!(report.unchanged, 2);

        // Removing a file prunes the row.
        std::fs::remove_file(dir.path().join("Artist - First.wav")).unwrap();
        let report = scanner.scan(dir.path(), None).await.unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(db.stats().await.unwrap().tracks, 1);
    }

    #[tokio::test]
    async fn rejects_missing_folder() {
        let db = Db::open_in_memory().await.unwrap();
        let scanner = Scanner::new(db);
        let err = scanner
            .scan("/definitely/not/here", None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Io { .. }));
    }
}
