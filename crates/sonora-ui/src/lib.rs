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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel, Weak};
use sonora_core::{
    format_duration, ArtworkFormat, Event, FolderTemplate, FormatPreference, FormatTier, Settings,
    TrackId, TrackView,
};
use sonora_db::{AlbumSummary, Db, HistoryEntry};
use sonora_playback::{Engine, PlaybackState, Resolver};
use sonora_scan::Scanner;
use tokio::runtime::Handle;
use tokio::sync::broadcast;

const RECENT_LIMIT: usize = 8;
const MOST_PLAYED_LIMIT: i64 = 8;
const HISTORY_LIMIT: i64 = 200;
const PAGE_LIMIT: i64 = 500;
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Transport artwork is tiny, so 96px is plenty even on a high-DPI screen.
const ARTWORK_PX: u32 = 96;

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
    ui.set_folder_hint(folder_hint().into());
    let busy = Arc::new(AtomicU64::new(0));
    let lookup = Arc::new(Mutex::new(TrackLookup::default()));

    let engine = build_engine(&lookup);
    // Restore the queue saved from the last session, if any.
    if let Some(engine) = engine.as_ref() {
        let saved = handle.block_on(ctx.db.load_queue());
        if let Ok(saved) = saved {
            engine.restore_from_snapshot(saved.snapshot());
        }
    }
    ui.set_playback_available(engine.is_some());
    if let Some(engine) = engine.as_ref() {
        refresh_queue_tabs(&weak, engine);
    }
    if let Some(engine) = engine.as_ref() {
        ui.set_volume(engine.volume() as f32);
    }
    if let Some(engine) = engine.clone() {
        wire_lyrics(&ui, weak.clone(), ctx.db.clone(), handle.clone(), engine);
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

    // Typing a path is a fallback; the picker is how people actually add one.
    #[cfg(not(target_os = "android"))]
    {
        let weak = weak.clone();
        let ctx = ctx.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_browse_folder(move || {
            let weak = weak.clone();
            let ctx = ctx.clone();
            let handle = handle.clone();
            let lookup = lookup.clone();
            handle.clone().spawn(async move {
                let picked = rfd::AsyncFileDialog::new()
                    .set_title("Choose a folder for Sonora to index")
                    .pick_folder()
                    .await;
                let Some(folder) = picked else {
                    return;
                };
                let path = folder.path().to_path_buf();
                let shown = path.display().to_string();
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_folder_path(shown.into()));
                start_scan(weak, ctx, handle, lookup, path);
            });
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

    {
        let weak = weak.clone();
        let db = ctx.db.clone();
        handle.spawn(async move {
            match db.load_settings().await {
                Ok(settings) => {
                    let first_run = !settings.onboarding_completed;
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        apply_settings(&ui, &settings);
                        if first_run {
                            ui.set_onboarding_step(0);
                            ui.set_onboarding_open(true);
                        }
                    });
                }
                Err(err) => log::error!("could not load settings: {err}"),
            }
        });
    }

    {
        let weak = weak.clone();
        let db = ctx.db.clone();
        let handle = handle.clone();
        ui.on_save_settings(
            move |tier,
                  bitrate,
                  cache_mib,
                  artwork_format,
                  quality,
                  template,
                  lastfm_key,
                  download_folder,
                  cache_folder,
                  device_sync| {
                let weak = weak.clone();
                let db = db.clone();
                handle.spawn(async move {
                    let mut settings = db.load_settings().await.unwrap_or_default();
                    settings.format = FormatPreference::new(
                        if tier.as_str() == "lossy" {
                            FormatTier::Lossy
                        } else {
                            FormatTier::Lossless
                        },
                        bitrate.clamp(0, i32::MAX) as u32,
                    );
                    settings.cache_limit_bytes = (cache_mib.max(0) as u64) * 1024 * 1024;
                    settings.artwork.format = if artwork_format.as_str() == "png" {
                        ArtworkFormat::Png
                    } else {
                        ArtworkFormat::Jpeg
                    };
                    settings.artwork.quality = quality.clamp(0, 255) as u8;
                    settings.folder_template = FolderTemplate::new(template.to_string());
                    settings.lastfm_api_key = Some(lastfm_key.to_string());
                    settings.download_folder = optional_path(download_folder.as_str());
                    settings.cache_folder = optional_path(cache_folder.as_str());
                    settings.device_sync_enabled = device_sync;
                    let settings = settings.normalized();

                    match db.save_settings(&settings).await {
                        Ok(()) => {
                            let saved = settings.clone();
                            let _ = weak.upgrade_in_event_loop(move |ui| {
                                apply_settings(&ui, &saved);
                                ui.set_status("Settings saved.".into());
                            });
                        }
                        Err(err) => {
                            log::error!("could not save settings: {err}");
                            set_status(&weak, format!("Could not save settings: {err}"));
                        }
                    }
                });
            },
        );
    }

    // Native folder pickers for the download and cache locations. Android has
    // no rfd backend, so the callbacks stay unregistered there and pressing
    // Browse is a no-op; the stored paths keep working on every platform.
    #[cfg(not(target_os = "android"))]
    {
        let weak = weak.clone();
        let handle = handle.clone();
        ui.on_pick_settings_folder(move |kind| {
            let weak = weak.clone();
            let is_cache = kind.as_str() == "cache";
            let handle = handle.clone();
            handle.spawn(async move {
                let title = if is_cache {
                    "Choose a folder for cached tracks"
                } else {
                    "Choose a folder for downloads"
                };
                let picked = rfd::AsyncFileDialog::new()
                    .set_title(title)
                    .pick_folder()
                    .await;
                let Some(folder) = picked else {
                    return;
                };
                let path = folder.path().display().to_string();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    if is_cache {
                        ui.set_settings_cache_folder(path.into());
                    } else {
                        ui.set_settings_download_folder(path.into());
                    }
                });
            });
        });
    }

    {
        let weak = weak.clone();
        ui.on_clear_settings_folder(move |kind| {
            if let Some(ui) = weak.upgrade() {
                if kind.as_str() == "cache" {
                    ui.set_settings_cache_folder("".into());
                } else {
                    ui.set_settings_download_folder("".into());
                }
            }
        });
    }

    {
        let weak = weak.clone();
        let ctx = ctx.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_favourite_toggled(move |id, favourite| {
            let weak = weak.clone();
            let ctx = ctx.clone();
            let lookup = lookup.clone();
            let handle_for_refresh = handle.clone();
            handle.spawn(async move {
                if let Err(err) = ctx.db.set_favourite(id as TrackId, favourite).await {
                    log::error!("could not update favourite: {err}");
                    return;
                }
                refresh_all(weak, ctx, handle_for_refresh, lookup);
            });
        });
    }

    {
        let weak = weak.clone();
        let ctx = ctx.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_rating_changed(move |id, current| {
            // Clicking cycles 0 → 1 → … → 5 → 0, so one control can both set
            // and clear a rating without a second button.
            let next = if !(0..5).contains(&current) {
                0
            } else {
                current + 1
            };
            let weak = weak.clone();
            let ctx = ctx.clone();
            let lookup = lookup.clone();
            let handle_for_refresh = handle.clone();
            handle.spawn(async move {
                if let Err(err) = ctx.db.set_rating(id as TrackId, next as u8).await {
                    log::error!("could not update rating: {err}");
                    return;
                }
                refresh_all(weak, ctx, handle_for_refresh, lookup);
            });
        });
    }

    {
        let weak = weak.clone();
        ui.on_onboarding_next(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_onboarding_step((ui.get_onboarding_step() + 1).min(3));
            }
        });
    }

    {
        let weak = weak.clone();
        ui.on_onboarding_back(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_onboarding_step((ui.get_onboarding_step() - 1).max(0));
            }
        });
    }

    {
        let weak = weak.clone();
        let db = ctx.db.clone();
        let handle = handle.clone();
        ui.on_onboarding_finish(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let tier = window.get_settings_format_tier().to_string();
            let bitrate = window.get_settings_bitrate().round() as i32;
            let lastfm = window.get_settings_lastfm_key().to_string();

            let weak = weak.clone();
            let db = db.clone();
            handle.spawn(async move {
                let base = db.load_settings().await.unwrap_or_default();
                let settings = onboarding_outcome(base, &tier, bitrate, &lastfm);
                match db.save_settings(&settings).await {
                    Ok(()) => {
                        let saved = settings.clone();
                        let _ = weak.upgrade_in_event_loop(move |ui| {
                            apply_settings(&ui, &saved);
                            ui.set_onboarding_open(false);
                            ui.set_status("Setup complete — add more folders any time.".into());
                        });
                    }
                    Err(err) => {
                        log::error!("could not save onboarding settings: {err}");
                        set_status(&weak, format!("Could not save setup: {err}"));
                    }
                }
            });
        });
    }

    {
        let weak = weak.clone();
        let db = ctx.db.clone();
        let handle = handle.clone();
        ui.on_run_onboarding(move || {
            let weak = weak.clone();
            let db = db.clone();
            let handle = handle.clone();
            handle.spawn(async move {
                match db.load_settings().await {
                    Ok(settings) => {
                        let _ = weak.upgrade_in_event_loop(move |ui| {
                            apply_settings(&ui, &settings);
                            ui.set_onboarding_step(0);
                            ui.set_onboarding_open(true);
                        });
                    }
                    Err(err) => log::error!("could not reload settings for onboarding: {err}"),
                }
            });
        });
    }

    let _timers = engine.clone().map(|engine| {
        let playback = attach_playback(
            &ui,
            weak.clone(),
            engine.clone(),
            lookup.clone(),
            ctx.db.clone(),
            handle.clone(),
        );
        let artwork = wire_artwork(weak.clone(), ctx.db.clone(), handle.clone(), engine);
        (playback, artwork)
    });

    listen_for_events(weak.clone(), ctx.events.subscribe(), handle.clone());
    refresh_all(weak.clone(), ctx.clone(), handle.clone(), lookup);

    ui.run()?;

    // Save the queue as it is right now so playback resumes on the next
    // launch. A shutdown in the middle of a track preserves the playing
    // track at the front of the restored queue.
    if let Some(engine) = engine.clone() {
        let snapshot = handle.block_on(async { engine.snapshot() });
        let _ = handle.block_on(ctx.db.save_queue_snapshot(&snapshot));
    }

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
    db: Db,
    handle: Handle,
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
        let db = db.clone();
        let handle = handle.clone();
        ui.on_next_track(move || {
            // `next` is only called from the transport, so pressing it while a
            // track is live is a skip. Natural gapless advances never come
            // through here; the timer records the incoming track instead.
            if engine.state() != PlaybackState::Stopped {
                if let Some(track) = engine.current() {
                    let db = db.clone();
                    handle.spawn(async move {
                        if let Err(err) = db.record_skip(track).await {
                            log::warn!("could not record a skip: {err}");
                        }
                    });
                }
            }
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
    let timer_db = db;
    let timer_handle = handle;
    let mut last_track = None;
    // Which track this listening session has already been counted, so pausing
    // and resuming does not inflate the play count.
    let mut recorded: Option<TrackId> = None;
    timer.start(TimerMode::Repeated, POLL_INTERVAL, move || {
        let Some(ui) = timer_weak.upgrade() else {
            return;
        };

        // Lyrics belong to the track that was playing when they were fetched, so
        // a new track clears the pane instead of showing the previous song's.
        let current = timer_engine.current();
        if current != last_track {
            last_track = current;
            recorded = None;
            if ui.get_lyrics_open() {
                ui.set_lyrics_text("".into());
                ui.set_lyrics_status("New track. Press Fetch lyrics.".into());
            }
        }

        // Record the play once the engine is actually playing it. A restored
        // queue has a current track before it is started, so state matters.
        let state = timer_engine.state();
        if state == PlaybackState::Playing {
            if let Some(track) = current {
                if recorded != Some(track) {
                    recorded = Some(track);
                    let db = timer_db.clone();
                    timer_handle.spawn(async move {
                        if let Err(err) = db.record_play(track).await {
                            log::warn!("could not record a play: {err}");
                        }
                    });
                }
            }
        }

        ui.set_playing(state == PlaybackState::Playing);
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
        let most_played = ctx
            .db
            .most_played(MOST_PLAYED_LIMIT)
            .await
            .unwrap_or_default();
        let favourites = ctx.db.favourites(PAGE_LIMIT).await.unwrap_or_default();
        let history = ctx.db.history(HISTORY_LIMIT, 0).await.unwrap_or_default();
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
        let all_rows = track_rows(&tracks);
        let most_played_rows = track_rows(&most_played);
        let favourite_rows = track_rows(&favourites);
        let history_rows = history_rows(&history);
        let album_rows = album_rows(&albums);
        let duration = format_duration(stats.total_duration_ms);

        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_track_count(stats.tracks as i32);
            ui.set_album_count(stats.albums as i32);
            ui.set_artist_count(stats.artists as i32);
            ui.set_library_duration(duration.into());
            ui.set_recent_tracks(model(recent_rows));
            ui.set_all_tracks(model(all_rows));
            ui.set_most_played_tracks(model(most_played_rows));
            ui.set_favourite_tracks(model(favourite_rows));
            ui.set_history_tracks(model(history_rows));
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

/// Wire the lyrics pane: read what is cached, fetch from LRCLIB on demand.
fn wire_lyrics(ui: &AppWindow, weak: Weak<AppWindow>, db: Db, handle: Handle, engine: Arc<Engine>) {
    {
        let weak = weak.clone();
        let db = db.clone();
        let handle = handle.clone();
        let engine = engine.clone();
        ui.on_lyrics_toggle(move || {
            let Some(window) = weak.clone().upgrade() else {
                return;
            };
            let open = !window.get_lyrics_open();
            window.set_lyrics_open(open);
            if !open {
                return;
            }

            window.set_queue_open(false);
            match engine.current() {
                Some(track) => load_lyrics(&weak, &db, &handle, track),
                None => {
                    window.set_lyrics_text("".into());
                    window.set_lyrics_status("Nothing playing.".into());
                }
            }
        });
    }

    {
        let weak = weak.clone();
        let db = db.clone();
        let handle = handle.clone();
        ui.on_lyrics_fetch(move || {
            let Some(window) = weak.clone().upgrade() else {
                return;
            };
            let Some(track) = engine.current() else {
                window.set_lyrics_status("Nothing playing.".into());
                return;
            };
            window.set_lyrics_status("Fetching from LRCLIB…".into());
            request_lyrics(&weak, db.clone(), handle.clone(), track);
        });
    }
}

/// Cover art beside the transport controls.
///
/// Playback also moves on its own (gapless advance, the next button), so the
/// artwork watches the engine's current track instead of a click callback.
fn wire_artwork(weak: Weak<AppWindow>, db: Db, handle: Handle, engine: Arc<Engine>) -> Timer {
    let timer = Timer::default();
    let timer_weak = weak.clone();
    let mut shown: Option<TrackId> = None;
    timer.start(TimerMode::Repeated, Duration::from_millis(500), move || {
        let Some(window) = timer_weak.upgrade() else {
            return;
        };

        let current = engine.current();
        if current == shown {
            return;
        }
        shown = current;

        match current {
            Some(track) => request_artwork(&weak, db.clone(), handle.clone(), track),
            None => window.set_artwork(slint::Image::default()),
        }
    });
    timer
}

/// Decode the track's embedded cover off the UI thread and hand it over.
fn request_artwork(weak: &Weak<AppWindow>, db: Db, handle: Handle, track: TrackId) {
    let weak = weak.clone();
    handle.spawn(async move {
        let Some(view) = db.track(track).await.ok().flatten() else {
            return;
        };
        let path = view.path.clone();
        // `slint::Image` is not `Send`, so the decode returns plain pixels and the
        // image is built on the UI thread.
        let pixels = tokio::task::spawn_blocking(move || cover_pixels(&path))
            .await
            .ok()
            .flatten();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let image = pixels.map_or_else(slint::Image::default, |(bytes, width, height)| {
                slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
                    &bytes, width, height,
                ))
            });
            ui.set_artwork(image);
        });
    });
}

/// Downscaled RGBA pixels of the file's embedded cover. A missing cover is
/// normal, so this returns `None` instead of an error.
fn cover_pixels(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let raw = sonora_artwork::extract_embedded(path).ok().flatten()?;
    let decoded = image::load_from_memory(&raw).ok()?;
    let thumb = decoded.thumbnail(ARTWORK_PX, ARTWORK_PX).to_rgba8();
    Some((thumb.as_raw().clone(), thumb.width(), thumb.height()))
}

/// Show whatever lyrics the database already has for `track`.
fn load_lyrics(weak: &Weak<AppWindow>, db: &Db, handle: &Handle, track: TrackId) {
    let weak = weak.clone();
    let db = db.clone();
    handle.spawn(async move {
        match db.lyrics_for(track).await {
            Ok(Some(stored)) => {
                let text = lyrics_text(stored.plain.as_deref(), stored.synced.as_deref());
                let status = format!("Cached ({})", stored.source);
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_lyrics_text(text.into());
                    ui.set_lyrics_status(status.into());
                });
            }
            Ok(None) => {
                let _ = weak.upgrade_in_event_loop(|ui| {
                    ui.set_lyrics_text("".into());
                    ui.set_lyrics_status("No saved lyrics yet. Press Fetch lyrics.".into());
                });
            }
            Err(err) => log::warn!("could not read stored lyrics: {err}"),
        }
    });
}

/// Ask LRCLIB for lyrics and keep them, so the next open is instant and offline.
fn request_lyrics(weak: &Weak<AppWindow>, db: Db, handle: Handle, track: TrackId) {
    let weak = weak.clone();
    handle.spawn(async move {
        let Some(view) = db.track(track).await.ok().flatten() else {
            let _ = weak.upgrade_in_event_loop(|ui| {
                ui.set_lyrics_status("That track is no longer in the library.".into())
            });
            return;
        };

        let query = sonora_metadata::TrackQuery {
            artist: view
                .artist
                .clone()
                .unwrap_or_else(|| "Unknown Artist".to_string()),
            title: view.title.clone(),
            album: view.album.clone(),
            duration_ms: Some(view.duration_ms.max(0) as u64),
        };

        // ureq is blocking, so the request runs off the async reactor.
        let result = tokio::task::spawn_blocking(move || {
            let http = sonora_metadata::UreqHttp::new(sonora_metadata::USER_AGENT);
            sonora_metadata::fetch_lyrics(&http, &query)
        })
        .await;

        match result {
            Ok(Ok(Some(lyrics))) => {
                if let Err(err) = db
                    .save_lyrics(
                        track,
                        lyrics.plain.as_deref(),
                        lyrics.synced.as_deref(),
                        "lrclib",
                    )
                    .await
                {
                    log::warn!("could not store lyrics for track {track}: {err}");
                }

                let text = lyrics_text(lyrics.plain.as_deref(), lyrics.synced.as_deref());
                let status = if lyrics.synced.is_some() {
                    "Synced lyrics from LRCLIB".to_string()
                } else {
                    "Lyrics from LRCLIB".to_string()
                };
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_lyrics_text(text.into());
                    ui.set_lyrics_status(status.into());
                });
            }
            Ok(Ok(None)) => {
                let _ = weak.upgrade_in_event_loop(|ui| {
                    ui.set_lyrics_status("LRCLIB has no lyrics for this track.".into())
                });
            }
            Ok(Err(err)) => {
                let status = format!("Lyrics lookup failed: {err}");
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_lyrics_status(status.into()));
            }
            Err(err) => {
                log::error!("lyrics task failed: {err}");
                let _ = weak.upgrade_in_event_loop(|ui| {
                    ui.set_lyrics_status("Lyrics lookup was interrupted.".into())
                });
            }
        }
    });
}

/// Text to display: plain lyrics when LRCLIB has them, otherwise the synced
/// file with its `[mm:ss.xx]` timestamps removed so it still reads as a song.
fn lyrics_text(plain: Option<&str>, synced: Option<&str>) -> String {
    if let Some(plain) = plain.filter(|text| !text.trim().is_empty()) {
        return plain.trim().to_string();
    }
    let Some(synced) = synced else {
        return String::new();
    };

    let mut lines = Vec::new();
    for line in synced.lines() {
        let mut rest = line.trim_start();
        while rest.starts_with('[') {
            match rest.find(']') {
                Some(end) => rest = rest[end + 1..].trim_start(),
                None => break,
            }
        }
        if !rest.is_empty() {
            lines.push(rest.to_string());
        }
    }
    lines.join("\n")
}

/// Apply the choices made in the onboarding flow to the stored settings.
///
/// The flow only owns three things: the format preference, the ceiling and the
/// Last.fm key. Everything else in the document is preserved, so running setup
/// again from Settings cannot wipe the cache budget or the folder template.
fn onboarding_outcome(mut base: Settings, tier: &str, bitrate: i32, lastfm: &str) -> Settings {
    base.format = FormatPreference::new(
        if tier == "lossy" {
            FormatTier::Lossy
        } else {
            FormatTier::Lossless
        },
        bitrate.max(0) as u32,
    );
    base.lastfm_api_key = Some(lastfm.to_string());
    base.onboarding_completed = true;
    base.normalized()
}

/// Where this machine usually keeps music. Windows has `USERPROFILE`, Linux has
/// `HOME`, and both conventionally use a `Music` folder under it, so the empty
/// path field shows something that could actually exist here.
fn folder_hint() -> String {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    match home {
        Some(home) => Path::new(&home).join("Music").display().to_string(),
        None => "Music".to_string(),
    }
}

/// Show the stored settings in the settings page.
fn apply_settings(ui: &AppWindow, settings: &Settings) {
    ui.set_settings_format_tier(
        match settings.format.tier {
            FormatTier::Lossless => "lossless",
            FormatTier::Lossy => "lossy",
        }
        .into(),
    );
    ui.set_settings_bitrate(settings.format.max_bitrate_kbps as f32);
    ui.set_settings_cache_mib((settings.cache_limit_bytes / 1024 / 1024) as f32);
    ui.set_settings_artwork_format(settings.artwork.format.extension().into());
    ui.set_settings_artwork_quality(i32::from(settings.artwork.quality));
    ui.set_settings_folder_template(settings.folder_template.as_str().into());
    ui.set_settings_lastfm_key(settings.lastfm_api_key.clone().unwrap_or_default().into());
    ui.set_settings_download_folder(
        settings
            .download_folder
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
            .into(),
    );
    ui.set_settings_cache_folder(
        settings
            .cache_folder
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
            .into(),
    );
    ui.set_settings_device_sync(settings.device_sync_enabled);
}

/// An empty path field means "use the default location", not "a folder named
/// the empty string".
fn optional_path(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
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

/// Keep the three queue-tab counts in sync with the engine state.
fn refresh_queue_tabs(weak: &Weak<AppWindow>, engine: &Engine) {
    let current = engine.current().is_some();
    let upcoming = engine.upcoming().len() as i32;
    let history = engine.history().len() as i32;
    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.set_current_count(if current { 1 } else { 0 });
        ui.set_upcoming_count(upcoming);
        ui.set_history_count(history);
    });
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

    refresh_queue_tabs(weak, engine);
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
            favourite: track.favourite,
            rating: track.rating.clamp(0, 5) as i32,
        })
        .collect()
}

/// History rows keep the track subtitle but add when it played and whether it
/// was skipped, so the log reads like a log rather than a duplicate of Tracks.
fn history_rows(entries: &[HistoryEntry]) -> Vec<TrackRow> {
    entries
        .iter()
        .map(|entry| {
            let mut subtitle = format!(
                "{} · {}",
                subtitle_for(&entry.track),
                relative_time(entry.played_at)
            );
            if entry.skipped {
                subtitle.push_str(" · skipped");
            }
            TrackRow {
                id: saturating_i32(entry.track.id as u64),
                title: entry.track.title.clone().into(),
                subtitle: subtitle.into(),
                duration: format_duration(entry.track.duration_ms).into(),
                favourite: entry.track.favourite,
                rating: entry.track.rating.clamp(0, 5) as i32,
            }
        })
        .collect()
}

/// Coarse age label; exact clock times are noise in a play history.
fn relative_time(played_at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(played_at);
    let seconds = (now - played_at).max(0);
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3_600 {
        format!("{} min ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} h ago", seconds / 3_600)
    } else {
        format!("{} d ago", seconds / 86_400)
    }
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
    fn plain_lyrics_are_shown_as_they_are() {
        let text = lyrics_text(Some("line one\nline two"), Some("[00:01.00] ignored"));
        assert_eq!(text, "line one\nline two");
    }

    #[test]
    fn synced_lyrics_lose_their_timestamps() {
        let synced =
            "[00:12.34] When you were here before\n[00:15.00] Couldn't look you in the eye\n\n";
        assert_eq!(
            lyrics_text(None, Some(synced)),
            "When you were here before\nCouldn't look you in the eye"
        );
    }

    #[test]
    fn missing_lyrics_produce_no_text() {
        assert_eq!(lyrics_text(None, None), "");
        assert_eq!(lyrics_text(Some("   "), None), "");
    }

    #[test]
    fn onboarding_keeps_settings_it_does_not_own() {
        let base = Settings {
            cache_limit_bytes: 8 * 1024 * 1024 * 1024,
            folder_template: FolderTemplate::new("{artist}/{title}"),
            ..Settings::default()
        };

        let finished = onboarding_outcome(base, "lossy", 192, "  key123 ");
        assert!(finished.onboarding_completed);
        assert_eq!(finished.format.tier, FormatTier::Lossy);
        assert_eq!(finished.format.max_bitrate_kbps, 192);
        assert_eq!(finished.lastfm_api_key.as_deref(), Some("key123"));
        assert_eq!(finished.cache_limit_bytes, 8 * 1024 * 1024 * 1024);
        assert_eq!(finished.folder_template.as_str(), "{artist}/{title}");
    }

    #[test]
    fn onboarding_clamps_a_stray_ceiling_and_blank_key() {
        let finished = onboarding_outcome(Settings::default(), "lossless", 9_000, "   ");
        assert_eq!(finished.format.max_bitrate_kbps, 1411);
        assert_eq!(finished.lastfm_api_key, None);
    }

    #[test]
    fn activating_a_track_that_is_not_on_screen_queues_nothing() {
        assert_eq!(queue_from(&[1, 2], 99), None);
        assert_eq!(queue_from(&[], 1), None);
    }
}
