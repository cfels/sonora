//! Slint bridge.
//!
//! The UI thread owns the Slint event loop; all database and filesystem work
//! happens on a Tokio runtime. Results are pushed back with
//! `upgrade_in_event_loop`, so the interface never blocks and never touches
//! `sqlx` directly.
//!
//! Playback follows the same rule: the window holds an `Engine` handle, queue
//! edits happen on the UI thread (they are vector operations), and a timer polls
//! the transport clocks so progress and state always reflect the real pipeline.

slint::include_modules!();

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel, Weak};
use sonora_core::{format_duration, Event, TrackId, TrackView};
use sonora_db::{AlbumSummary, Db};
use sonora_playback::{Engine, PlaybackState, Resolver};
use sonora_scan::Scanner;
use tokio::runtime::Handle;
use tokio::sync::broadcast;

const RECENT_LIMIT: usize = 8;
const PAGE_LIMIT: i64 = 500;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

pub struct AppContext {
    pub db: Db,
    pub scanner: Scanner,
    pub events: broadcast::Sender<Event>,
}

/// What the UI needs to play a track and label it in the queue.
#[derive(Default)]
struct TrackLookup {
    paths: HashMap<TrackId, PathBuf>,
    labels: HashMap<TrackId, (String, String)>,
    /// The list currently on screen, in display order.
    visible: Vec<TrackId>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    let lookup = Arc::new(Mutex::new(TrackLookup::default()));

    let engine = build_engine(&lookup);
    ui.set_playback_available(engine.is_some());
    if let Some(engine) = engine.as_ref() {
        ui.set_volume(engine.volume() as f32);
    }

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
        let lookup = lookup.clone();
        ui.on_search_changed(move |text| {
            run_search(
                weak.clone(),
                ctx.clone(),
                handle.clone(),
                lookup.clone(),
                text.to_string(),
                busy.clone(),
            );
        });
    }

    {
        let weak = weak.clone();
        let ctx = ctx.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
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
            start_scan(
                weak.clone(),
                ctx.clone(),
                handle.clone(),
                lookup.clone(),
                path,
            );
        });
    }

    {
        let weak = weak.clone();
        let ctx = ctx.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_rescan_requested(move || {
            let weak = weak.clone();
            let ctx = ctx.clone();
            let lookup = lookup.clone();
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
                refresh_all(weak.clone(), ctx.clone(), handle_for_task, lookup.clone());
            });
        });
    }

    let _playback_timer = engine.clone().map(|engine| {
        attach_playback(&ui, weak.clone(), engine, lookup.clone());
    });

    listen_for_events(weak.clone(), ctx.events.subscribe(), handle.clone());
    refresh_all(weak.clone(), ctx.clone(), handle.clone(), lookup);

    ui.run()?;
    Ok(())
}

/// Build the playback engine, or report why the transport is unavailable.
///
/// A missing GStreamer installation must not stop the library from working, so a
/// failure disables the transport bar instead of aborting the application.
fn build_engine(lookup: &Arc<Mutex<TrackLookup>>) -> Option<Arc<Engine>> {
    let resolver_lookup = lookup.clone();
    let resolver: Resolver =
        Box::new(move |track| lock(&resolver_lookup).paths.get(&track).cloned());

    match Engine::new(resolver) {
        Ok(engine) => Some(Arc::new(engine)),
        Err(err) => {
            log::error!("playback unavailable: {err}");
            None
        }
    }
}

fn attach_playback(
    ui: &AppWindow,
    weak: Weak<AppWindow>,
    engine: Arc<Engine>,
    lookup: Arc<Mutex<TrackLookup>>,
) -> Timer {
    {
        let engine = engine.clone();
        let lookup = lookup.clone();
        let weak = weak.clone();
        ui.on_track_activated(move |id| {
            let tracks = {
                let lookup = lock(&lookup);
                queue_from(&lookup.visible, id as TrackId)
            };

            let Some(tracks) = tracks else {
                return;
            };
            if let Err(err) = engine.play_tracks(&tracks) {
                log::warn!("could not start playback: {err}");
                return;
            }
            refresh_playback(&weak, &engine, &lookup);
        });
    }

    {
        let engine = engine.clone();
        let lookup = lookup.clone();
        let weak = weak.clone();
        ui.on_play_next_requested(move |id| {
            engine.play_next(id as TrackId);
            refresh_playback(&weak, &engine, &lookup);
        });
    }

    {
        let engine = engine.clone();
        let lookup = lookup.clone();
        let weak = weak.clone();
        ui.on_enqueue_requested(move |id| {
            engine.add_to_queue(id as TrackId);
            refresh_playback(&weak, &engine, &lookup);
        });
    }

    {
        let engine = engine.clone();
        let weak = weak.clone();
        ui.on_toggle_playback(move || {
            if let Err(err) = engine.toggle() {
                log::warn!("could not toggle playback: {err}");
            }
            set_playing(&weak, engine.state() == PlaybackState::Playing);
        });
    }

    {
        let engine = engine.clone();
        let lookup = lookup.clone();
        let weak = weak.clone();
        ui.on_previous_track(move || {
            if let Err(err) = engine.previous() {
                log::warn!("could not go back: {err}");
            }
            refresh_playback(&weak, &engine, &lookup);
        });
    }

    {
        let engine = engine.clone();
        let lookup = lookup.clone();
        let weak = weak.clone();
        ui.on_next_track(move || {
            if let Err(err) = engine.next() {
                log::warn!("could not skip: {err}");
            }
            refresh_playback(&weak, &engine, &lookup);
        });
    }

    {
        let engine = engine.clone();
        ui.on_seek_requested(move |position| {
            if let Err(err) = engine.seek_ms(position.max(0) as u64) {
                log::warn!("could not seek: {err}");
            }
        });
    }

    {
        let engine = engine.clone();
        ui.on_volume_changed(move |volume| {
            engine.set_volume(f64::from(volume));
        });
    }

    {
        let engine = engine.clone();
        let lookup = lookup.clone();
        let weak = weak.clone();
        ui.on_queue_move(move |from, to| {
            if engine.move_upcoming(from.max(0) as usize, to.max(0) as usize) {
                refresh_playback(&weak, &engine, &lookup);
            }
        });
    }

    {
        let engine = engine.clone();
        let lookup = lookup.clone();
        let weak = weak.clone();
        ui.on_queue_remove(move |index| {
            if engine.remove_upcoming(index.max(0) as usize).is_some() {
                refresh_playback(&weak, &engine, &lookup);
            }
        });
    }

    let timer = Timer::default();
    let timer_weak = weak;
    let timer_engine = engine;
    timer.start(TimerMode::Repeated, POLL_INTERVAL, move || {
        let Some(ui) = timer_weak.upgrade() else {
            return;
        };
        ui.set_playing(timer_engine.state() == PlaybackState::Playing);
        let position = timer_engine.position_ms().unwrap_or(0);
        let duration = timer_engine.duration_ms().unwrap_or(0);
        ui.set_position_ms(saturating_i32(position));
        ui.set_duration_ms(saturating_i32(duration));
        ui.set_position_label(format_duration(position as i64).into());
        ui.set_duration_label(format_duration(duration as i64).into());
    });
    timer
}

fn start_scan(
    weak: Weak<AppWindow>,
    ctx: Arc<AppContext>,
    handle: Handle,
    lookup: Arc<Mutex<TrackLookup>>,
    folder: PathBuf,
) {
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
        refresh_all(weak_for_task, ctx_for_task, handle_for_refresh, lookup);
    });
}

fn refresh_all(
    weak: Weak<AppWindow>,
    ctx: Arc<AppContext>,
    handle: Handle,
    lookup: Arc<Mutex<TrackLookup>>,
) {
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

        remember(&lookup, &tracks);

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
    lookup: Arc<Mutex<TrackLookup>>,
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

        remember(&lookup, &rows);

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

fn set_playing(weak: &Weak<AppWindow>, playing: bool) {
    let _ = weak.upgrade_in_event_loop(move |ui| ui.set_playing(playing));
}

/// Push the queue and the now-playing labels to the window.
fn refresh_playback(weak: &Weak<AppWindow>, engine: &Engine, lookup: &Arc<Mutex<TrackLookup>>) {
    let entries = queue_rows(engine, lookup);
    let (title, subtitle) = now_playing(engine, lookup);

    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.set_queue_entries(model(entries));
        ui.set_now_playing_title(title.into());
        ui.set_now_playing_subtitle(subtitle.into());
    });
}

fn now_playing(engine: &Engine, lookup: &Arc<Mutex<TrackLookup>>) -> (String, String) {
    let current = engine.current();
    let lookup = lock(lookup);
    current
        .and_then(|track| lookup.labels.get(&track).cloned())
        .unwrap_or_else(|| ("Nothing playing".to_string(), String::new()))
}

fn queue_rows(engine: &Engine, lookup: &Arc<Mutex<TrackLookup>>) -> Vec<QueueRow> {
    let lookup = lock(lookup);
    engine
        .upcoming()
        .into_iter()
        .map(|track| {
            let (title, subtitle) = lookup
                .labels
                .get(&track)
                .cloned()
                .unwrap_or_else(|| (format!("Track {track}"), String::new()));
            QueueRow {
                title: title.into(),
                subtitle: subtitle.into(),
            }
        })
        .collect()
}

/// Record the list on screen so clicking a row can queue everything below it.
fn remember(lookup: &Arc<Mutex<TrackLookup>>, tracks: &[TrackView]) {
    let mut lookup = lock(lookup);
    for track in tracks {
        lookup.paths.insert(track.id, track.path.clone());
        lookup
            .labels
            .insert(track.id, (track.title.clone(), subtitle_for(track)));
    }
    lookup.visible = tracks.iter().map(|track| track.id).collect();
}

/// The ids that should be queued when `track` is double-clicked in a list:
/// the track itself plus everything below it, matching how desktop players
/// behave when you start from the middle of an album.
fn queue_from(visible: &[TrackId], track: TrackId) -> Option<Vec<TrackId>> {
    let index = visible.iter().position(|candidate| *candidate == track)?;
    Some(visible[index..].to_vec())
}

fn saturating_i32(value: u64) -> i32 {
    value.min(i32::MAX as u64) as i32
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
            id: saturating_i32(track.id as u64),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activating_a_track_queues_it_and_everything_below() {
        let visible = vec![10, 20, 30, 40];
        assert_eq!(queue_from(&visible, 30), Some(vec![30, 40]));
        assert_eq!(queue_from(&visible, 30).unwrap().len(), 2);
    }

    #[test]
    fn activating_a_track_that_is_not_on_screen_queues_nothing() {
        assert_eq!(queue_from(&[1, 2], 99), None);
        assert_eq!(queue_from(&[], 1), None);
    }
}
