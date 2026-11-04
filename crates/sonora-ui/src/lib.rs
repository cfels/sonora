//! Slint bridge.
//!
//! The UI thread owns the Slint event loop; all database and filesystem work
//! happens on a Tokio runtime. Results are pushed back with
//! `upgrade_in_event_loop`, so the interface never blocks and never touches
//! `sqlx` directly.

slint::include_modules!();

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel, Weak};
use sonora_core::{format_duration, Event, TrackView};
use sonora_db::{AlbumSummary, Db};
use sonora_scan::Scanner;
use tokio::runtime::Handle;
use tokio::sync::broadcast;

const RECENT_LIMIT: usize = 8;
const PAGE_LIMIT: i64 = 500;

pub struct AppContext {
    pub db: Db,
    pub scanner: Scanner,
    pub events: broadcast::Sender<Event>,
}

/// Start the application. Blocks until the window is closed.
pub fn run(db: Db, handle: Handle) -> anyhow::Result<()> {
    let (events, _) = broadcast::channel(256);
    let ctx = Arc::new(AppContext {
        scanner: Scanner::new(db.clone()),
        db,
        events,
    });

    let ui = AppWindow::new()?;
    let weak = ui.as_weak();
    let busy = Arc::new(AtomicU64::new(0));

    {
        let weak = weak.clone();
        ui.on_section_changed(move |section| {
            if let Some(ui) = weak.upgrade() {
                ui.set_section(section);
            }
        });
    }

    {
        let weak = weak.clone();
        let ctx = ctx.clone();
        let handle = handle.clone();
        ui.on_search_changed(move |text| {
            run_search(
                weak.clone(),
                ctx.clone(),
                handle.clone(),
                text.to_string(),
                busy.clone(),
            );
        });
    }

    {
        let weak = weak.clone();
        let ctx = ctx.clone();
        let handle = handle.clone();
        ui.on_add_folder(move |folder| {
            let folder = folder.trim().to_string();
            if folder.is_empty() {
                set_status(&weak, "Enter a folder path first.");
                return;
            }
            let path = PathBuf::from(&folder);
            if !path.is_dir() {
                set_status(&weak, format!("{folder} is not a folder Sonora can read."));
                return;
            }
            start_scan(weak.clone(), ctx.clone(), handle.clone(), path);
        });
    }

    {
        let weak = weak.clone();
        let ctx = ctx.clone();
        let handle = handle.clone();
        ui.on_rescan_requested(move || {
            let weak = weak.clone();
            let ctx = ctx.clone();
            let handle_for_task = handle.clone();
            handle.spawn(async move {
                set_status(&weak, "Scanning library…");
                match ctx.scanner.scan_all(Some(ctx.events.clone())).await {
                    Ok(reports) => {
                        let stats = ctx.db.stats().await.unwrap_or_default();
                        log::info!("rescan finished for {} folders", reports.len());
                        set_status(
                            &weak,
                            format!(
                                "{} tracks · {} albums · {} artists",
                                stats.tracks, stats.albums, stats.artists
                            ),
                        );
                    }
                    Err(err) => set_status(&weak, format!("Scan failed: {err}")),
                }
                refresh_all(weak.clone(), ctx.clone(), handle_for_task);
            });
        });
    }

    listen_for_events(weak.clone(), ctx.events.subscribe(), handle.clone());
    refresh_all(weak.clone(), ctx.clone(), handle.clone());

    ui.run()?;
    Ok(())
}

fn start_scan(weak: Weak<AppWindow>, ctx: Arc<AppContext>, handle: Handle, folder: PathBuf) {
    let weak_for_task = weak.clone();
    let ctx_for_task = ctx.clone();
    let handle_for_refresh = handle.clone();
    handle.spawn(async move {
        set_status(&weak_for_task, format!("Scanning {}…", folder.display()));
        set_busy(&weak_for_task, true);

        match ctx_for_task
            .scanner
            .scan(&folder, Some(ctx_for_task.events.clone()))
            .await
        {
            Ok(report) => {
                log::info!(
                    "scan complete: imported={} updated={} removed={} errors={}",
                    report.imported,
                    report.updated,
                    report.removed,
                    report.total_errors()
                );
                if report.total_errors() > 0 {
                    set_status(
                        &weak_for_task,
                        format!(
                            "Indexed {} files ({} added), {} could not be read",
                            report.seen,
                            report.imported,
                            report.total_errors()
                        ),
                    );
                }
            }
            Err(err) => {
                log::error!("scan failed: {err}");
                set_status(&weak_for_task, format!("Scan failed: {err}"));
            }
        }

        set_busy(&weak_for_task, false);
        refresh_all(weak_for_task, ctx_for_task, handle_for_refresh);
    });
}

fn refresh_all(weak: Weak<AppWindow>, ctx: Arc<AppContext>, handle: Handle) {
    handle.spawn(async move {
        let stats = match ctx.db.stats().await {
            Ok(stats) => stats,
            Err(err) => {
                log::error!("could not read library statistics: {err}");
                return;
            }
        };
        let recent = ctx
            .db
            .recently_added(RECENT_LIMIT as i64)
            .await
            .unwrap_or_default();
        let tracks = ctx.db.list_tracks(PAGE_LIMIT, 0).await.unwrap_or_default();
        let albums = ctx.db.albums().await.unwrap_or_default();
        let folders: Vec<slint::SharedString> = ctx
            .db
            .folders()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(_, path)| path.to_string_lossy().to_string().into())
            .collect();

        let recent_rows = track_rows(&recent);
        let track_rows = track_rows(&tracks);
        let album_rows = album_rows(&albums);
        let duration = format_duration(stats.total_duration_ms);

        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_track_count(stats.tracks as i32);
            ui.set_album_count(stats.albums as i32);
            ui.set_artist_count(stats.artists as i32);
            ui.set_library_duration(duration.into());
            ui.set_recent_tracks(model(recent_rows));
            ui.set_all_tracks(model(track_rows));
            ui.set_albums(model(album_rows));
            ui.set_folders(model(folders));
        });
    });
}

fn run_search(
    weak: Weak<AppWindow>,
    ctx: Arc<AppContext>,
    handle: Handle,
    query: String,
    generation: Arc<AtomicU64>,
) {
    let current = generation.fetch_add(1, Ordering::SeqCst) + 1;
    handle.spawn(async move {
        let rows = if query.trim().is_empty() {
            ctx.db.list_tracks(PAGE_LIMIT, 0).await
        } else {
            ctx.db.search(&query, PAGE_LIMIT).await
        };

        let rows = match rows {
            Ok(rows) => rows,
            Err(err) => {
                log::error!("search failed: {err}");
                return;
            }
        };

        // A later keystroke already produced a newer query; drop this result.
        if generation.load(Ordering::SeqCst) != current {
            return;
        }

        let rows = track_rows(&rows);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_all_tracks(model(rows));
        });
    });
}

fn listen_for_events(
    weak: Weak<AppWindow>,
    mut receiver: broadcast::Receiver<Event>,
    handle: Handle,
) {
    handle.spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(Event::ScanProgress(progress)) => {
                    set_status(
                        &weak,
                        format!(
                            "Scanning — {} files, {} added, {} errors",
                            progress.seen, progress.imported, progress.errors
                        ),
                    );
                }
                Ok(Event::ScanStarted { folder }) => {
                    set_busy(&weak, true);
                    set_status(&weak, format!("Scanning {}…", folder.display()));
                }
                Ok(Event::ScanFinished(report)) => {
                    set_busy(&weak, false);
                    set_status(
                        &weak,
                        format!(
                            "Scan finished — {} files, {} new, {} removed, {} errors",
                            report.seen,
                            report.imported,
                            report.removed,
                            report.total_errors()
                        ),
                    );
                }
                Ok(Event::Warn(message)) => set_status(&weak, message),
                Ok(Event::LibraryChanged(_)) => {}
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    log::warn!("UI missed {skipped} engine events");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn set_status(weak: &Weak<AppWindow>, message: impl Into<slint::SharedString>) {
    let message = message.into();
    let _ = weak.upgrade_in_event_loop(move |ui| ui.set_status(message));
}

fn set_busy(weak: &Weak<AppWindow>, busy: bool) {
    let _ = weak.upgrade_in_event_loop(move |ui| ui.set_busy(busy));
}

/// Slint needs an `Rc`-backed model; building it inside the event-loop closure
/// keeps the reference off the async threads.
fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::from(std::rc::Rc::new(VecModel::from(items)))
}

fn track_rows(tracks: &[TrackView]) -> Vec<TrackRow> {
    tracks
        .iter()
        .map(|track| TrackRow {
            title: track.title.clone().into(),
            subtitle: subtitle_for(track).into(),
            duration: format_duration(track.duration_ms).into(),
        })
        .collect()
}

fn subtitle_for(track: &TrackView) -> String {
    match (track.artist.as_deref(), track.album.as_deref()) {
        (Some(artist), Some(album)) => format!("{artist} · {album}"),
        (Some(artist), None) => artist.to_string(),
        (None, Some(album)) => format!("Unknown Artist · {album}"),
        (None, None) => "Unknown Artist".to_string(),
    }
}

fn album_rows(albums: &[AlbumSummary]) -> Vec<AlbumRow> {
    albums
        .iter()
        .map(|album| {
            let meta = match album.year {
                Some(year) => format!(
                    "{} tracks · {} · {year}",
                    album.track_count,
                    format_duration(album.duration_ms)
                ),
                None => format!(
                    "{} tracks · {}",
                    album.track_count,
                    format_duration(album.duration_ms)
                ),
            };
            AlbumRow {
                title: album.title.clone().into(),
                subtitle: album.album_artist.clone().into(),
                meta: meta.into(),
            }
        })
        .collect()
}
