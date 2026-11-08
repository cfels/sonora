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

mod row_artwork;
mod transport;
use transport::{PlaybackHandle, PlaybackWorker};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};
use sonora_core::{
    format_duration, AlbumId, ArtistId, ArtworkFormat, Event, FolderTemplate, FormatPreference,
    FormatTier, PlaylistId, PlaylistRules, Settings, TrackId, TrackView, SMART_LIMIT_DEFAULT,
};
use sonora_db::{AlbumSummary, Db, HistoryEntry, PlaylistSummary};
use sonora_playback::{Engine, PlaybackState, Resolver};
use sonora_scan::Scanner;
use tokio::runtime::Handle;
use tokio::sync::broadcast;

const RECENT_LIMIT: usize = 8;
const MOST_PLAYED_LIMIT: i64 = 8;
const HISTORY_LIMIT: i64 = 200;
const PAGE_LIMIT: i64 = 500;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// The design uses a 250px cover; 512px also covers double-density displays.
const ARTWORK_PX: u32 = 512;
const QUEUE_ARTWORK_PX: u32 = 128;
const BACKDROP_WIDTH: u32 = 480;
const BACKDROP_HEIGHT: u32 = 270;
type CoverPixels = (Vec<u8>, u32, u32);

pub struct AppContext {
    pub db: Db,
    pub scanner: Scanner,
    pub events: broadcast::Sender<Event>,
}

/// Display and navigation info cached for tracks the UI has seen, so the
/// transport and queue can label and link them without re-querying the database.
#[derive(Clone, Default)]
struct TrackInfo {
    title: String,
    subtitle: String,
    artist: String,
    artist_id: i64,
    album_id: i64,
}

/// What the UI needs to play a track and label it in the queue.
#[derive(Default)]
struct TrackLookup {
    paths: HashMap<TrackId, PathBuf>,
    labels: HashMap<TrackId, TrackInfo>,
    /// ReplayGain adjustments for the tracks currently on screen, in dB.
    /// A value of `None` means the track carries no ReplayGain metadata.
    gains: HashMap<TrackId, Option<f32>>,
    /// The list currently on screen, in display order.
    visible: Vec<TrackId>,
    /// The section on screen, so a refresh reloads the page being looked at
    /// rather than every page that happens to still have state.
    section: String,
    track_limit: i64,
    /// The album page, when one is open. Kept apart from `visible` so opening an
    /// album does not change how the other pages queue.
    album: Option<AlbumPage>,
    /// The artist page, when one is open. Also kept apart from `visible`.
    artist: Option<ArtistPage>,
    /// The playlist page, when one is open. Also kept apart from `visible`.
    playlist: Option<PlaylistPage>,
}

/// The album currently drilled into, with its tracks in album order.
#[derive(Clone)]
struct AlbumPage {
    id: AlbumId,
    order: Vec<TrackId>,
}

/// The artist currently drilled into, with everything they perform in play order.
#[derive(Clone)]
struct ArtistPage {
    id: ArtistId,
    order: Vec<TrackId>,
}

/// The playlist currently open, with its tracks in the listener's order.
#[derive(Clone)]
struct PlaylistPage {
    id: PlaylistId,
    order: Vec<TrackId>,
}

/// Everything the open playlist's page shows, built from the summary, its tracks
/// and (for a smart playlist) its rules, so opening and refreshing a page cannot
/// disagree about what the page looks like.
struct PlaylistView {
    name: String,
    meta: String,
    rows: Vec<TrackRow>,
    smart: bool,
    rules_summary: String,
    filter: String,
    favourites: bool,
    stars: i32,
    year: String,
}

impl PlaylistView {
    fn new(summary: &PlaylistSummary, tracks: &[TrackView], rules: Option<&PlaylistRules>) -> Self {
        let total_ms: i64 = tracks.iter().map(|track| track.duration_ms).sum();
        Self {
            name: summary.name.clone(),
            meta: format!("{} tracks · {}", tracks.len(), format_duration(total_ms)),
            rows: track_rows(tracks),
            smart: rules.is_some(),
            rules_summary: rules.map(PlaylistRules::describe).unwrap_or_default(),
            filter: rules.map(|rules| rules.text.clone()).unwrap_or_default(),
            favourites: rules.map(|rules| rules.favourites_only).unwrap_or(false),
            stars: rules.map(|rules| i32::from(rules.min_rating)).unwrap_or(0),
            year: rules
                .and_then(|rules| rules.year_from)
                .map(|year| year.to_string())
                .unwrap_or_default(),
        }
    }

    /// Push the page to the window. The rename field is set separately, because
    /// a refresh must not overwrite a name the listener is still typing.
    fn apply(&self, ui: &AppWindow) {
        ui.set_playlist_title(self.name.clone().into());
        ui.set_playlist_meta(self.meta.clone().into());
        ui.set_playlist_tracks(model(self.rows.clone()));
        ui.set_playlist_smart(self.smart);
        ui.set_playlist_rules_summary(self.rules_summary.clone().into());
        ui.set_smart_filter_input(self.filter.clone().into());
        ui.set_smart_favourites(self.favourites);
        ui.set_smart_min_rating(self.stars);
        ui.set_smart_year_input(self.year.clone().into());
    }
}

/// Read the editor's year box. Blank means "no bound"; anything that is not a
/// four-digit year is reported instead of silently ignored, because a typo would
/// otherwise widen or narrow the playlist without saying so.
fn parse_year(text: &str) -> std::result::Result<Option<i64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    match text.parse::<i64>() {
        Ok(year) if (1000..=9999).contains(&year) => Ok(Some(year)),
        _ => Err(format!(
            "\"{text}\" is not a year. Use four digits, like 1971."
        )),
    }
}

/// Turn the rule editor into a stored document. The editor is compact (one year
/// bound, a star floor), so an unset field simply means "no constraint".
fn rules_from_editor(
    filter: String,
    favourites: bool,
    min_rating: i32,
    year: &str,
) -> std::result::Result<PlaylistRules, String> {
    Ok(PlaylistRules {
        text: filter,
        min_rating: min_rating.clamp(0, 5) as u8,
        favourites_only: favourites,
        year_from: parse_year(year)?,
        year_to: None,
        limit: SMART_LIMIT_DEFAULT,
    }
    .normalized())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Start the application. Blocks until the window is closed.
pub fn run(db: Db, handle: Handle, data_dir: PathBuf) -> anyhow::Result<()> {
    let (events, _) = broadcast::channel(256);
    let ctx = Arc::new(AppContext {
        scanner: Scanner::new(db.clone()),
        db,
        events,
    });

    let ui = AppWindow::new()?;
    let weak = ui.as_weak();
    ui.set_folder_hint(folder_hint().into());
    let search_generation = Arc::new(AtomicU64::new(0));
    let lookup = Arc::new(Mutex::new(TrackLookup::default()));

    let engine = build_engine(&lookup);
    // Restore the queue saved from the last session, if any.
    if let Some(engine) = engine.as_ref() {
        let saved = handle.block_on(ctx.db.load_queue());
        if let Ok(saved) = saved {
            let ids: Vec<TrackId> = saved
                .current()
                .into_iter()
                .chain(saved.upcoming_tracks())
                .chain(saved.history_tracks())
                .collect();
            let tracks = handle.block_on(async {
                let mut tracks = Vec::new();
                for id in ids {
                    if let Some(track) = ctx.db.track(id).await? {
                        tracks.push(track);
                    }
                }
                Ok::<_, sonora_core::Error>(tracks)
            })?;
            remember_tracks(&lookup, &tracks);
            let has_current = saved.current().is_some();
            engine.restore_from_snapshot(saved.snapshot());
            if has_current {
                ui.set_section("player".into());
                lock(&lookup).section = "player".to_string();
            }
        }
    }
    ui.set_playback_available(engine.is_some());
    if engine.is_none() {
        ui.set_status("Playback unavailable. Check the GStreamer installation.".into());
    }
    if let Some(engine) = engine.as_ref() {
        refresh_queue_tabs(&weak, engine);
    }
    if let Some(engine) = engine.as_ref() {
        ui.set_volume(engine.volume() as f32);
    }

    let settings = handle.block_on(ctx.db.load_settings())?;
    if let Some(engine) = engine.as_ref() {
        engine.set_volume(settings.volume);
        engine.set_muted(settings.muted);
        ui.set_volume(settings.volume as f32);
        ui.set_muted(settings.muted);
        if let Some(name) = settings.playback_device.as_deref() {
            if let Err(err) = engine.set_device(Some(name)) {
                log::warn!("saved audio output unavailable: {err}");
                ui.set_status(
                    format!("Saved audio output unavailable; using system default: {err}").into(),
                );
            }
        }
    }

    // Wire the playback-device picker. Each callback takes its own clones so the
    // three registrations never share ownership.
    {
        let handle = handle.clone();
        let weak = weak.clone();
        ui.on_pick_playback_device(move || {
            let handle = handle.clone();
            let weak = weak.clone();
            handle.spawn(async move {
                let sinks = Engine::available_sinks();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    let devices: Vec<slint::SharedString> =
                        sinks.into_iter().map(Into::into).collect();
                    ui.set_devices(model(devices));
                    ui.set_device_picker_selected(0);
                    ui.set_device_picker_open(true);
                });
            });
        });
    }

    {
        let db = ctx.db.clone();
        let handle = handle.clone();
        let weak = weak.clone();
        ui.on_device_picker_select(move |device| {
            let db = db.clone();
            let handle = handle.clone();
            let weak = weak.clone();
            let selected = device.to_string();
            handle.spawn(async move {
                if selected.is_empty() {
                    let _ = weak.upgrade_in_event_loop(|ui| {
                        ui.set_device_picker_open(false);
                    });
                    return;
                }
                let mut settings = db.load_settings().await.unwrap_or_default();
                settings.playback_device = Some(selected.clone());
                if db.save_settings(&settings).await.is_err() {
                    let _ = weak.upgrade_in_event_loop(|ui| {
                        ui.set_status("Could not save the playback device.".into());
                    });
                    return;
                }
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_settings_device(selected.into());
                    ui.set_device_picker_open(false);
                    ui.set_status("Audio output saved. Restart Sonora to apply it.".into());
                });
            });
        });
    }

    {
        let handle = handle.clone();
        let weak = weak.clone();
        ui.on_device_picker_cancel(move || {
            let handle = handle.clone();
            let weak = weak.clone();
            handle.spawn(async move {
                let _ = weak.upgrade_in_event_loop(|ui| {
                    ui.set_device_picker_open(false);
                });
            });
        });
    }

    let appearance_writer = wire_appearance(&ui, weak.clone(), ctx.db.clone(), handle.clone());

    let mut playback_worker = engine
        .as_ref()
        .map(|engine| PlaybackWorker::new(weak.clone(), engine.clone(), lookup.clone()))
        .transpose()?;
    let playback = playback_worker
        .as_ref()
        .map(|worker| worker.playback.clone());
    row_artwork::wire(&ui, weak.clone(), lookup.clone(), handle.clone());
    ui.global::<RowArtwork>()
        .set_playback_available(engine.is_some());

    // Timed lyrics live beside the pane: the loader fills them, the playback
    // poll reads them to move the highlight line by line.
    let timed_lyrics = LyricsLoader::default();

    if let Some(engine) = engine.clone() {
        wire_lyrics(
            &ui,
            weak.clone(),
            ctx.db.clone(),
            handle.clone(),
            engine,
            timed_lyrics.clone(),
        );
    }

    // Playlists work with or without a transport; only playing one needs the
    // engine, and `wire_playlists` skips those two handlers when it is missing.
    wire_playlists(
        &ui,
        weak.clone(),
        ctx.db.clone(),
        handle.clone(),
        lookup.clone(),
        playback.clone(),
    );

    {
        let weak = weak.clone();
        let lookup = lookup.clone();
        ui.on_section_changed(move |section| {
            // Leaving the album and artist pages forgets them, so later refreshes
            // do not reload a page nobody is looking at. The two pages point back
            // at each other, so both survive while either one is open.
            {
                let mut lookup = lock(&lookup);
                if section.as_str() != "album" && section.as_str() != "artist" {
                    lookup.album = None;
                    lookup.artist = None;
                }
                if section.as_str() != "playlist" {
                    lookup.playlist = None;
                }
                lookup.section = section.to_string();
            }
            if let Some(ui) = weak.upgrade() {
                // The name field is shared by "create" and "rename", so arriving on
                // the list page starts it empty instead of pre-filled with the last
                // playlist that was opened.
                if section.as_str() == "playlists" {
                    ui.set_playlist_name_input("".into());
                }
                ui.set_playlist_delete_armed(false);
                ui.set_section(section);
            }
        });
    }

    {
        let weak = weak.clone();
        let ctx = ctx.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        let generation = search_generation.clone();
        ui.on_search_changed(move |text| {
            run_search(
                weak.clone(),
                ctx.clone(),
                handle.clone(),
                lookup.clone(),
                text.to_string(),
                generation.clone(),
            );
        });
    }

    {
        let weak = weak.clone();
        let ctx = ctx.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        let generation = search_generation.clone();
        ui.on_load_more_tracks(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            {
                let mut lookup = lock(&lookup);
                lookup.track_limit = lookup
                    .track_limit
                    .max(PAGE_LIMIT)
                    .saturating_add(PAGE_LIMIT);
            }
            run_search(
                weak.clone(),
                ctx.clone(),
                handle.clone(),
                lookup.clone(),
                window.get_search_text().to_string(),
                generation.clone(),
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
        ui.on_remove_folder(move |id| {
            let weak = weak.clone();
            let ctx = ctx.clone();
            let handle_for_refresh = handle.clone();
            let lookup = lookup.clone();
            handle.spawn(async move {
                match ctx.db.remove_folder(i64::from(id)).await {
                    Ok(()) => {
                        set_status(
                            &weak,
                            "Folder removed from the library. Files on disk were kept.",
                        );
                        refresh_all(weak, ctx, handle_for_refresh, lookup);
                    }
                    Err(err) => set_status(&weak, format!("Could not remove folder: {err}")),
                }
            });
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
        let engine = engine.clone();
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
                  device_sync,
                  device,
                  retention| {
                let weak = weak.clone();
                let db = db.clone();
                let mixer = engine
                    .as_ref()
                    .map(|engine| (engine.volume(), engine.muted()));
                let appearance = weak.upgrade().map(|ui| AppearanceSettings {
                    blur_panels: ui.get_settings_blur_panels(),
                    cover_background: ui.get_settings_cover_background(),
                });
                handle.spawn(async move {
                    let mut settings = match db.load_settings().await {
                        Ok(settings) => settings,
                        Err(err) => {
                            set_status(&weak, format!("Could not read settings: {err}"));
                            return;
                        }
                    };
                    if let Some(appearance) = appearance {
                        settings.appearance = appearance;
                    }
                    if let Some((volume, muted)) = mixer {
                        settings.volume = volume;
                        settings.muted = muted;
                    }
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
                    settings.history_retention =
                        HistoryRetention::from_index(retention.max(0) as usize);
                    settings.playback_device = optional_string(device.as_str());
                    let settings = settings.normalized();

                    match db.save_settings(&settings).await {
                        Ok(()) => {
                            let saved = settings.clone();
                            let history = match db.history(HISTORY_LIMIT, 0).await {
                                Ok(history) => history,
                                Err(err) => {
                                    set_status(
                                        &weak,
                                        format!(
                                            "Settings saved, but history refresh failed: {err}"
                                        ),
                                    );
                                    return;
                                }
                            };
                            let _ = weak.upgrade_in_event_loop(move |ui| {
                                apply_settings(&ui, &saved);
                                ui.set_history_tracks(model(history_rows(&history)));
                                ui.invoke_cache_action(false);
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

    {
        let weak = weak.clone();
        let db = ctx.db.clone();
        let handle = handle.clone();
        let default_root = data_dir.join("cache");
        let engine = engine.clone();
        ui.on_cache_action(move |clear| {
            let weak = weak.clone();
            let db = db.clone();
            let default_root = default_root.clone();
            let engine = engine.clone();
            handle.spawn(async move {
                let settings = match db.load_settings().await {
                    Ok(settings) => settings,
                    Err(err) => { set_status(&weak, format!("Could not read cache settings: {err}")); return; }
                };
                let root = settings.cache_folder.unwrap_or(default_root);
                let cache = sonora_cache::TrackCache::at(root.clone());
                let protected: Vec<TrackId> = engine.as_ref().map(|engine| {
                    engine.current().into_iter().chain(engine.upcoming()).collect()
                }).unwrap_or_default();
                let mut originals = Vec::new();
                if clear {
                    let entries = match tokio::task::spawn_blocking(move || cache.entries()).await {
                        Ok(Ok(entries)) => entries,
                        Ok(Err(err)) => { set_status(&weak, format!("Could not inspect cache: {err}")); return; }
                        Err(err) => { set_status(&weak, format!("Cache task failed: {err}")); return; }
                    };
                    for entry in entries {
                        match db.track(entry.track).await {
                            Ok(Some(track)) => originals.push(track.path),
                            Ok(None) => {},
                            Err(err) => { set_status(&weak, format!("Could not protect library files: {err}")); return; }
                        }
                    }
                }
                let cache = sonora_cache::TrackCache::at(root);
                let result = tokio::task::spawn_blocking(move || {
                    let report = if clear { Some(cache.clear(&protected, &originals)?) } else { None };
                    Ok::<_, sonora_core::Error>((cache.stats()?, report))
                }).await;
                match result {
                    Ok(Ok((stats, report))) => {
                        let label = format!("{:.2} MiB · {} cached files", stats.bytes as f64 / 1048576.0, stats.tracks);
                        let _ = weak.upgrade_in_event_loop(move |ui| {
                            ui.set_cache_size(label.into());
                            if let Some(report) = report {
                                ui.set_status(format!("Cleared {} cache files; kept {} protected or unowned files. {}",
                                    report.removed, report.kept, report.errors.join("; ")).into());
                            }
                        });
                    }
                    Ok(Err(err)) => set_status(&weak, format!("Cache operation failed: {err}")),
                    Err(err) => set_status(&weak, format!("Cache task failed: {err}")),
                }
            });
        });
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

    let _timers = playback.clone().map(|playback| {
        let playback_timer = attach_playback(
            &ui,
            weak.clone(),
            playback.clone(),
            lookup.clone(),
            ctx.db.clone(),
            handle.clone(),
            timed_lyrics.clone(),
        );
        let artwork = wire_artwork(
            weak.clone(),
            ctx.db.clone(),
            handle.clone(),
            playback.engine.clone(),
        );
        (playback_timer, artwork)
    });

    listen_for_events(weak.clone(), ctx.events.subscribe(), handle.clone());
    refresh_all(weak.clone(), ctx.clone(), handle.clone(), lookup);

    ui.run()?;
    if let Some(worker) = playback_worker.as_mut() {
        worker.shutdown()?;
    }
    ui.on_appearance_changed(|_, _| {});
    handle.block_on(appearance_writer)?;
    handle.block_on(ctx.db.save_appearance(AppearanceSettings {
        blur_panels: ui.get_settings_blur_panels(),
        cover_background: ui.get_settings_cover_background(),
    }))?;

    // Save the queue as it is right now so playback resumes on the next
    // launch. A shutdown in the middle of a track preserves the playing
    // track at the front of the restored queue.
    if let Some(engine) = engine.clone() {
        let snapshot = handle.block_on(async { engine.snapshot() });
        handle.block_on(ctx.db.save_queue_snapshot(&snapshot))?;
        handle.block_on(ctx.db.save_volume(engine.volume(), engine.muted()))?;
    }

    Ok(())
}

fn wire_appearance(
    ui: &AppWindow,
    weak: Weak<AppWindow>,
    db: Db,
    handle: Handle,
) -> tokio::task::JoinHandle<()> {
    let (sender, mut changes) = tokio::sync::watch::channel(None::<AppearanceSettings>);
    ui.on_appearance_changed(move |blur_panels, cover_background| {
        sender.send_replace(Some(AppearanceSettings {
            blur_panels,
            cover_background,
        }));
    });
    handle.spawn(async move {
        while changes.changed().await.is_ok() {
            let appearance = *changes.borrow_and_update();
            if let Some(appearance) = appearance {
                if let Err(err) = db.save_appearance(appearance).await {
                    set_status(&weak, format!("Could not save appearance: {err}"));
                }
            }
        }
    })
}

fn optional_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Build the playback engine, or report why the transport is unavailable.
///
/// A missing GStreamer installation must not stop the library from working, so a
/// failure disables the transport bar instead of aborting the application.
fn build_engine(lookup: &Arc<Mutex<TrackLookup>>) -> Option<Arc<Engine>> {
    let resolver_lookup = lookup.clone();
    let resolver: Resolver =
        Box::new(move |track| lock(&resolver_lookup).paths.get(&track).cloned());

    let engine = match Engine::new(resolver) {
        Ok(engine) => engine,
        Err(err) => {
            log::error!("playback unavailable: {err}");
            return None;
        }
    };

    // Wire the ReplayGain resolver so the engine can apply per-track gain,
    // including during gapless handover (which the engine drives internally).
    let gain_lookup = lookup.clone();
    engine.set_gain_resolver(Box::new(move |track| {
        lock(&gain_lookup).gains.get(&track).copied().flatten()
    }));

    Some(Arc::new(engine))
}

fn attach_playback(
    ui: &AppWindow,
    weak: Weak<AppWindow>,
    playback: PlaybackHandle,
    lookup: Arc<Mutex<TrackLookup>>,
    db: Db,
    handle: Handle,
    timed: LyricsLoader,
) -> Timer {
    let engine = playback.engine.clone();
    {
        let lookup = lookup.clone();
        let weak = weak.clone();
        let playback = playback.clone();
        ui.on_track_activated(move |id| {
            let tracks = {
                let lookup = lock(&lookup);
                queue_from(&lookup.visible, id as TrackId).or_else(|| {
                    lookup
                        .paths
                        .contains_key(&(id as TrackId))
                        .then_some(vec![id as TrackId])
                })
            };

            let Some(tracks) = tracks else {
                return;
            };
            if let Err(err) = playback.play_tracks(&tracks) {
                set_status(&weak, format!("Could not start playback: {err}"));
            }
        });
    }

    {
        let lookup = lookup.clone();
        let weak = weak.clone();
        let playback = playback.clone();
        ui.on_album_track_activated(move |id| {
            let tracks = {
                let lookup = lock(&lookup);
                let order = lookup.album.as_ref().map(|page| page.order.clone());
                order.and_then(|order| queue_from(&order, id as TrackId))
            };

            let Some(tracks) = tracks else {
                return;
            };
            if let Err(err) = playback.play_tracks(&tracks) {
                set_status(&weak, format!("Could not start the album: {err}"));
            }
        });
    }

    {
        let lookup = lookup.clone();
        let weak = weak.clone();
        let playback = playback.clone();
        ui.on_play_album(move || {
            let tracks = lock(&lookup)
                .album
                .as_ref()
                .map(|page| page.order.clone())
                .unwrap_or_default();
            if tracks.is_empty() {
                return;
            }
            if let Err(err) = playback.play_tracks(&tracks) {
                set_status(&weak, format!("Could not start the album: {err}"));
            }
        });
    }

    {
        let weak = weak.clone();
        let db = db.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_open_album(move |id| {
            let weak = weak.clone();
            let db = db.clone();
            let handle = handle.clone();
            let lookup = lookup.clone();
            let album_id = id as AlbumId;
            handle.spawn(async move {
                let album = match db.album(album_id).await {
                    Ok(Some(album)) => album,
                    Ok(None) => {
                        let _ = weak.upgrade_in_event_loop(|ui| {
                            ui.set_status("That album is no longer in the library.".into());
                        });
                        return;
                    }
                    Err(err) => {
                        log::error!("could not open album {album_id}: {err}");
                        let _ = weak.upgrade_in_event_loop(|ui| {
                            ui.set_status("Could not open that album.".into());
                        });
                        return;
                    }
                };
                let tracks = db.album_tracks(album_id).await.unwrap_or_default();

                remember_tracks(&lookup, &tracks);
                let total_ms: i64 = tracks.iter().map(|track| track.duration_ms).sum();
                let counts = format!("{} tracks · {}", tracks.len(), format_duration(total_ms));
                let order: Vec<TrackId> = tracks.iter().map(|track| track.id).collect();
                let rows = track_rows(&tracks);
                // Coming from an artist page, Back returns there; otherwise Back
                // leads out to the album list.
                let back = if lock(&lookup).artist.is_some() {
                    "artist"
                } else {
                    "albums"
                };
                {
                    let mut lookup = lock(&lookup);
                    lookup.section = "album".to_string();
                    lookup.album = Some(AlbumPage {
                        id: album_id,
                        order,
                    });
                }

                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_album_title(album.title.into());
                    ui.set_album_artist(album.album_artist.into());
                    ui.set_album_artist_id(saturating_i32(album.album_artist_id as u64));
                    ui.set_album_meta(counts.into());
                    ui.set_album_back(back.into());
                    ui.set_album_tracks(model(rows));
                    ui.set_section("album".into());
                });
            });
        });
    }

    {
        let lookup = lookup.clone();
        let weak = weak.clone();
        let playback = playback.clone();
        ui.on_artist_track_activated(move |id| {
            let tracks = {
                let lookup = lock(&lookup);
                let order = lookup.artist.as_ref().map(|page| page.order.clone());
                order.and_then(|order| queue_from(&order, id as TrackId))
            };

            let Some(tracks) = tracks else {
                return;
            };
            if let Err(err) = playback.play_tracks(&tracks) {
                set_status(&weak, format!("Could not start the artist: {err}"));
            }
        });
    }

    {
        let lookup = lookup.clone();
        let weak = weak.clone();
        let playback = playback.clone();
        ui.on_play_artist(move || {
            let tracks = lock(&lookup)
                .artist
                .as_ref()
                .map(|page| page.order.clone())
                .unwrap_or_default();
            if tracks.is_empty() {
                return;
            }
            if let Err(err) = playback.play_tracks(&tracks) {
                set_status(&weak, format!("Could not start the artist: {err}"));
            }
        });
    }

    {
        let weak = weak.clone();
        let db = db.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_open_artist(move |id, name| {
            let weak = weak.clone();
            let db = db.clone();
            let handle = handle.clone();
            let lookup = lookup.clone();
            let name = name.to_string();
            handle.clone().spawn(async move {
                let artist_id = id as ArtistId;
                let albums = db.artist_albums(artist_id).await.unwrap_or_default();
                let tracks = match db.artist_tracks(artist_id).await {
                    Ok(tracks) => tracks,
                    Err(err) => {
                        log::error!("could not open artist {artist_id}: {err}");
                        let _ = weak.upgrade_in_event_loop(|ui| {
                            ui.set_status("Could not open that artist.".into());
                        });
                        return;
                    }
                };

                remember_tracks(&lookup, &tracks);
                let total_ms: i64 = tracks.iter().map(|track| track.duration_ms).sum();
                let meta = format!(
                    "{} albums · {} tracks · {}",
                    albums.len(),
                    tracks.len(),
                    format_duration(total_ms)
                );
                let order: Vec<TrackId> = tracks.iter().map(|track| track.id).collect();
                let album_rows = album_rows(&albums);
                let rows = track_rows(&tracks);
                // The album page keeps its state while an artist page is open, so
                // Back can return to the release the artist was opened from.
                let back = if lock(&lookup).album.is_some() {
                    "album"
                } else if lock(&lookup).section == "artists" {
                    "artists"
                } else {
                    "albums"
                };
                {
                    let mut lookup = lock(&lookup);
                    lookup.section = "artist".to_string();
                    lookup.artist = Some(ArtistPage {
                        id: artist_id,
                        order,
                    });
                }

                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_artist_name(name.into());
                    ui.set_artist_meta(meta.into());
                    ui.set_artist_back(back.into());
                    ui.set_artist_albums(model(album_rows));
                    ui.set_artist_tracks(model(rows));
                    ui.set_section("artist".into());
                });
            });
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
        let weak = weak.clone();
        let playback = playback.clone();
        ui.on_toggle_playback(move || {
            if let Err(err) = playback.toggle() {
                set_status(&weak, format!("Could not start playback: {err}"));
            }
        });
    }

    {
        let weak = weak.clone();
        let playback = playback.clone();
        ui.on_previous_track(move || {
            if let Err(err) = playback.previous() {
                set_status(&weak, format!("Could not go back: {err}"));
            }
        });
    }

    {
        let engine = engine.clone();
        let weak = weak.clone();
        let db = db.clone();
        let handle = handle.clone();
        let playback = playback.clone();
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
            if let Err(err) = playback.next() {
                set_status(&weak, format!("Could not skip: {err}"));
            }
        });
    }

    {
        let engine = engine.clone();
        let weak = weak.clone();
        ui.on_seek_requested(move |position| {
            if let Err(err) = engine.seek_ms(position.max(0) as u64) {
                set_status(&weak, format!("Could not seek: {err}"));
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

    {
        let engine = engine.clone();
        ui.on_mute_toggled(move || engine.set_muted(!engine.muted()));
    }
    {
        let engine = engine.clone();
        ui.on_shuffle_toggled(move || engine.set_shuffle(!engine.shuffle()));
    }
    {
        let engine = engine.clone();
        ui.on_repeat_toggled(move || {
            engine.set_repeat(match engine.repeat() {
                sonora_core::RepeatMode::Off => sonora_core::RepeatMode::All,
                sonora_core::RepeatMode::All => sonora_core::RepeatMode::One,
                sonora_core::RepeatMode::One => sonora_core::RepeatMode::Off,
            })
        });
    }
    let timer = Timer::default();
    let timer_weak = weak;
    let timer_engine = engine;
    let timer_db = db;
    let timer_handle = handle;
    let timer_timed = timed.clone();
    let mut last_track = None;
    let mut last_volume = (timer_engine.volume(), timer_engine.muted());
    let mut volume_changed_at = None;
    let mut cleanup_at = Instant::now();
    // Which track this listening session has already been counted, so pausing
    // and resuming does not inflate the play count.
    let mut recorded: Option<(TrackId, u64)> = None;
    timer.start(TimerMode::Repeated, POLL_INTERVAL, move || {
        let Some(ui) = timer_weak.upgrade() else {
            return;
        };

        // Lyrics belong to the track that was playing when they were fetched, so
        // a new track clears the pane instead of showing the previous song's.
        let current = timer_engine.current();
        let pending = ui.get_pending_track();
        if current != last_track
            && (pending == 0 || current.map_or(0, |id| saturating_i32(id.max(0) as u64)) == pending)
        {
            last_track = current;
            ui.global::<RowArtwork>()
                .set_current_track(current.map_or(0, |track| saturating_i32(track.max(0) as u64)));
            refresh_playback(&timer_weak, &timer_engine, &lookup);
            if ui.get_lyrics_open() {
                if let Some(track) = current {
                    load_lyrics(
                        &timer_weak,
                        &timer_db,
                        &timer_handle,
                        track,
                        timer_timed.clone(),
                    );
                } else {
                    show_lyrics(&ui, &timer_timed.timed, "", None);
                    ui.set_lyrics_status("Nothing playing.".into());
                    ui.set_lyrics_track(0);
                }
            }
        }

        // Record the play once the engine is actually playing it. A restored
        // queue has a current track before it is started, so state matters.
        let state = timer_engine.state();
        if state == PlaybackState::Playing {
            let (audible, serial) = timer_engine.active_session();
            if let Some(track) = audible {
                if recorded != Some((track, serial)) {
                    recorded = Some((track, serial));
                    let db = timer_db.clone();
                    let weak = timer_weak.clone();
                    timer_handle.spawn(async move {
                        if let Err(err) = db.record_play(track).await {
                            set_status(&weak, format!("Could not record play: {err}"));
                            return;
                        }
                        let result = tokio::try_join!(
                            db.history(HISTORY_LIMIT, 0),
                            db.most_played(MOST_PLAYED_LIMIT)
                        );
                        match result {
                            Ok((history, most_played)) => {
                                let _ = weak.upgrade_in_event_loop(move |ui| {
                                    ui.set_history_tracks(model(history_rows(&history)));
                                    ui.set_most_played_tracks(model(track_rows(&most_played)));
                                });
                            }
                            Err(err) => {
                                set_status(&weak, format!("Could not refresh play activity: {err}"))
                            }
                        }
                    });
                }
            }
        }

        ui.set_playing(state == PlaybackState::Playing);
        ui.set_seekable(timer_engine.is_seekable());
        ui.set_volume(timer_engine.volume() as f32);
        ui.set_muted(timer_engine.muted());
        ui.set_shuffle(timer_engine.shuffle());
        ui.set_repeat(match timer_engine.repeat() {
            sonora_core::RepeatMode::Off => 0,
            sonora_core::RepeatMode::All => 1,
            sonora_core::RepeatMode::One => 2,
        });
        if let Some(error) = timer_engine.take_error() {
            ui.set_status(error.into());
        }
        let volume = (timer_engine.volume(), timer_engine.muted());
        if volume != last_volume {
            last_volume = volume;
            volume_changed_at = Some(Instant::now());
        }
        if volume_changed_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(600)) {
            volume_changed_at = None;
            let db = timer_db.clone();
            let weak = timer_weak.clone();
            timer_handle.spawn(async move {
                if let Err(err) = db.save_volume(volume.0, volume.1).await {
                    set_status(&weak, format!("Could not save volume: {err}"));
                }
            });
        }
        if cleanup_at.elapsed() >= Duration::from_secs(60) {
            cleanup_at = Instant::now();
            let db = timer_db.clone();
            let weak = timer_weak.clone();
            timer_handle.spawn(async move {
                match db.prune_history().await {
                    Ok(removed) if removed > 0 => match db.history(HISTORY_LIMIT, 0).await {
                        Ok(entries) => {
                            let _ = weak.upgrade_in_event_loop(move |ui| {
                                ui.set_history_tracks(model(history_rows(&entries)))
                            });
                        }
                        Err(err) => set_status(&weak, format!("Could not refresh history: {err}")),
                    },
                    Ok(_) => {}
                    Err(err) => set_status(&weak, format!("History cleanup failed: {err}")),
                }
            });
        }
        let position = timer_engine.position_ms().unwrap_or(0);
        let duration = timer_engine.duration_ms().unwrap_or(0);
        if !ui.get_seeking() {
            ui.set_position_ms(saturating_i32(position));
        }
        ui.set_duration_ms(saturating_i32(duration));
        ui.set_position_label(format_duration(position as i64).into());
        ui.set_duration_label(format_duration(duration as i64).into());

        // Synced lyrics follow the transport: the line whose timestamp has been
        // passed is the one shown as current, and -1 before the first line.
        let active = {
            let lines = lock(&timer_timed.timed);
            current_lyric_index(&lines, position)
        };
        ui.set_lyric_active(active);
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

/// Wire playlists: create, rename, delete, open one, and take tracks in and out.
///
/// Each handler edits the database and then reloads only the playlist list and,
/// when one is open, its tracks — a playlist edit never rebuilds the library.
fn wire_playlists(
    ui: &AppWindow,
    weak: Weak<AppWindow>,
    db: Db,
    handle: Handle,
    lookup: Arc<Mutex<TrackLookup>>,
    playback: Option<PlaybackHandle>,
) {
    {
        let weak = weak.clone();
        let db = db.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_playlist_open(move |id| {
            let weak = weak.clone();
            let db = db.clone();
            let handle = handle.clone();
            let lookup = lookup.clone();
            handle.spawn(show_playlist(id as PlaylistId, weak, db, lookup));
        });
    }

    {
        let weak = weak.clone();
        let db = db.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_playlist_create(move |name| {
            let weak = weak.clone();
            let db = db.clone();
            let handle = handle.clone();
            let lookup = lookup.clone();
            let name = name.to_string();
            handle.clone().spawn(async move {
                match db.create_playlist(&name).await {
                    Ok(_) => {
                        set_status(&weak, format!("Created playlist \"{}\".", name.trim()));
                        let _ =
                            weak.upgrade_in_event_loop(|ui| ui.set_playlist_name_input("".into()));
                        refresh_playlists(&weak, &db, &handle, &lookup);
                    }
                    Err(err) => {
                        log::warn!("could not create a playlist: {err}");
                        set_status(&weak, "A playlist needs a name.");
                    }
                }
            });
        });
    }

    {
        let weak = weak.clone();
        let db = db.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_playlist_smart_create(move |name, filter, favourites, stars, year| {
            let weak = weak.clone();
            let db = db.clone();
            let handle = handle.clone();
            let lookup = lookup.clone();
            let name = name.to_string();
            let filter = filter.to_string();
            let year = year.to_string();
            handle.clone().spawn(async move {
                let rules = match rules_from_editor(filter, favourites, stars, &year) {
                    Ok(rules) => rules,
                    Err(message) => {
                        set_status(&weak, &message);
                        return;
                    }
                };
                match db.create_smart_playlist(&name, &rules).await {
                    Ok(id) => {
                        set_status(
                            &weak,
                            format!("Created smart playlist \"{}\".", name.trim()),
                        );
                        let _ = weak.upgrade_in_event_loop(|ui| ui.set_smart_name_input("".into()));
                        // Open it, so the listener sees what the rules picked
                        // instead of a new row with nothing to explain it.
                        show_playlist(id, weak.clone(), db.clone(), lookup.clone()).await;
                        refresh_playlists(&weak, &db, &handle, &lookup);
                    }
                    Err(err) => {
                        log::warn!("could not create a smart playlist: {err}");
                        set_status(&weak, "A playlist needs a name.");
                    }
                }
            });
        });
    }

    {
        let weak = weak.clone();
        let db = db.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_playlist_smart_save(move |filter, favourites, stars, year| {
            let weak = weak.clone();
            let db = db.clone();
            let handle = handle.clone();
            let lookup = lookup.clone();
            let filter = filter.to_string();
            let year = year.to_string();
            handle.clone().spawn(async move {
                let Some(page) = lock(&lookup).playlist.clone() else {
                    set_status(&weak, "Open a smart playlist before saving its rules.");
                    return;
                };
                let rules = match rules_from_editor(filter, favourites, stars, &year) {
                    Ok(rules) => rules,
                    Err(message) => {
                        set_status(&weak, &message);
                        return;
                    }
                };
                match db.set_playlist_rules(page.id, &rules).await {
                    Ok(()) => {
                        set_status(&weak, format!("Rules saved: {}.", rules.describe()));
                        refresh_playlists(&weak, &db, &handle, &lookup);
                    }
                    Err(err) => {
                        log::error!("could not save the rules of playlist {}: {err}", page.id);
                        set_status(&weak, "Only a smart playlist can hold rules.");
                    }
                }
            });
        });
    }

    {
        let weak = weak.clone();
        let db = db.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_playlist_rename(move |name| {
            let weak = weak.clone();
            let db = db.clone();
            let handle = handle.clone();
            let lookup = lookup.clone();
            let name = name.to_string();
            handle.clone().spawn(async move {
                let Some(playlist) = lock(&lookup).playlist.clone() else {
                    set_status(&weak, "Open a playlist before renaming one.");
                    return;
                };
                match db.rename_playlist(playlist.id, &name).await {
                    Ok(()) => {
                        set_status(&weak, format!("Renamed to \"{}\".", name.trim()));
                        refresh_playlists(&weak, &db, &handle, &lookup);
                    }
                    Err(err) => {
                        log::warn!("could not rename playlist {}: {err}", playlist.id);
                        set_status(&weak, "A playlist needs a name.");
                    }
                }
            });
        });
    }

    {
        let weak = weak.clone();
        let db = db.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_playlist_delete(move || {
            let weak = weak.clone();
            let db = db.clone();
            let handle = handle.clone();
            let lookup = lookup.clone();
            handle.clone().spawn(async move {
                let Some(playlist) = lock(&lookup).playlist.clone() else {
                    return;
                };
                match db.delete_playlist(playlist.id).await {
                    Ok(()) => {
                        {
                            let mut lookup = lock(&lookup);
                            lookup.playlist = None;
                            lookup.section = "playlists".to_string();
                        }
                        set_status(&weak, "Playlist deleted. Its tracks stay in the library.");
                        let _ = weak.upgrade_in_event_loop(|ui| ui.set_section("playlists".into()));
                        refresh_playlists(&weak, &db, &handle, &lookup);
                    }
                    Err(err) => {
                        log::error!("could not delete playlist {}: {err}", playlist.id);
                        set_status(&weak, "Could not delete that playlist.");
                    }
                }
            });
        });
    }

    {
        let weak = weak.clone();
        let db = db.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_playlist_track_remove(move |track| {
            let weak = weak.clone();
            let db = db.clone();
            let handle = handle.clone();
            let lookup = lookup.clone();
            handle.clone().spawn(async move {
                let Some(playlist) = lock(&lookup).playlist.clone() else {
                    return;
                };
                match db.remove_from_playlist(playlist.id, track as TrackId).await {
                    Ok(true) => set_status(&weak, "Removed from the playlist."),
                    Ok(false) => set_status(&weak, "That track was not in the playlist."),
                    Err(err) => {
                        log::error!("could not remove track {track}: {err}");
                        set_status(&weak, "Could not remove that track.");
                    }
                }
                refresh_playlists(&weak, &db, &handle, &lookup);
            });
        });
    }

    {
        let weak = weak.clone();
        let db = db.clone();
        let handle = handle.clone();
        let lookup = lookup.clone();
        ui.on_playlist_add(move |playlist, track| {
            let weak = weak.clone();
            let db = db.clone();
            let handle = handle.clone();
            let lookup = lookup.clone();
            handle.clone().spawn(async move {
                let playlist_id = playlist as PlaylistId;
                let track_id = track as TrackId;
                match db.add_to_playlist(playlist_id, track_id).await {
                    Ok(true) => set_status(&weak, "Added to the playlist."),
                    Ok(false) => set_status(&weak, "That track is already in that playlist."),
                    Err(err) => {
                        log::error!(
                            "could not add track {track_id} to playlist {playlist_id}: {err}"
                        );
                        set_status(&weak, "Could not add that track.");
                    }
                }
                let _ = weak.upgrade_in_event_loop(|ui| ui.set_playlist_picker_open(false));
                refresh_playlists(&weak, &db, &handle, &lookup);
            });
        });
    }

    {
        let weak = weak.clone();
        ui.on_playlist_picker_close(move || {
            let _ = weak.upgrade_in_event_loop(|ui| ui.set_playlist_picker_open(false));
        });
    }

    if let Some(playback) = playback {
        {
            let playback = playback.clone();
            let lookup = lookup.clone();
            let weak = weak.clone();
            ui.on_playlist_play(move || {
                let tracks = lock(&lookup)
                    .playlist
                    .as_ref()
                    .map(|page| page.order.clone())
                    .unwrap_or_default();
                if tracks.is_empty() {
                    return;
                }
                if let Err(err) = playback.play_tracks(&tracks) {
                    set_status(&weak, format!("Could not start the playlist: {err}"));
                }
            });
        }

        {
            let playback = playback.clone();
            let lookup = lookup.clone();
            let weak = weak.clone();
            ui.on_playlist_track_activated(move |id| {
                let tracks = {
                    let lookup = lock(&lookup);
                    let order = lookup.playlist.as_ref().map(|page| page.order.clone());
                    order.and_then(|order| queue_from(&order, id as TrackId))
                };

                let Some(tracks) = tracks else {
                    return;
                };
                if let Err(err) = playback.play_tracks(&tracks) {
                    set_status(&weak, format!("Could not start the playlist: {err}"));
                }
            });
        }
    }
}

/// Open a playlist page, whether its tracks come from entries or from rules.
///
/// Opening is also where the rule editor is filled in, so a smart playlist's
/// page always starts from what is actually stored.
async fn show_playlist(
    id: PlaylistId,
    weak: Weak<AppWindow>,
    db: Db,
    lookup: Arc<Mutex<TrackLookup>>,
) {
    let Some(summary) = db.playlist(id).await.ok().flatten() else {
        set_status(&weak, "That playlist no longer exists.");
        return;
    };
    let tracks = match db.playlist_tracks(id).await {
        Ok(tracks) => tracks,
        Err(err) => {
            log::error!("could not read playlist {id}: {err}");
            set_status(&weak, "Could not open that playlist.");
            return;
        }
    };
    let rules = match db.playlist_rules(id).await {
        Ok(rules) => rules,
        Err(err) => {
            log::error!("could not read the rules of playlist {id}: {err}");
            set_status(&weak, "Could not read that playlist's rules.");
            return;
        }
    };

    remember_tracks(&lookup, &tracks);
    let view = PlaylistView::new(&summary, &tracks, rules.as_ref());
    let name = summary.name.clone();
    let order: Vec<TrackId> = tracks.iter().map(|track| track.id).collect();
    {
        let mut lookup = lock(&lookup);
        lookup.section = "playlist".to_string();
        lookup.playlist = Some(PlaylistPage { id, order });
    }

    let _ = weak.upgrade_in_event_loop(move |ui| {
        view.apply(&ui);
        ui.set_playlist_name_input(name.into());
        ui.set_playlist_delete_armed(false);
        ui.set_section("playlist".into());
    });
}

/// Reload the playlist list, and the open playlist's tracks when there is one.
fn refresh_playlists(
    weak: &Weak<AppWindow>,
    db: &Db,
    handle: &Handle,
    lookup: &Arc<Mutex<TrackLookup>>,
) {
    let weak = weak.clone();
    let db = db.clone();
    let lookup = lookup.clone();
    handle.spawn(async move {
        let playlists = match db.playlists().await {
            Ok(playlists) => playlists,
            Err(err) => {
                log::error!("could not read playlists: {err}");
                return;
            }
        };
        let rows = playlist_rows(&playlists);
        let open = lock(&lookup).playlist.clone();
        let page = match open {
            Some(page) => {
                let tracks = db.playlist_tracks(page.id).await.unwrap_or_default();
                remember_tracks(&lookup, &tracks);
                let order: Vec<TrackId> = tracks.iter().map(|track| track.id).collect();
                if let Some(open) = lock(&lookup).playlist.as_mut() {
                    open.order = order;
                }
                let summary = playlists.iter().find(|playlist| playlist.id == page.id);
                let rules = db.playlist_rules(page.id).await.ok().flatten();
                summary.map(|summary| PlaylistView::new(summary, &tracks, rules.as_ref()))
            }
            None => None,
        };

        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_playlists(model(rows));
            if let Some(view) = page {
                view.apply(&ui);
            }
        });
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
        let track_limit = lock(&lookup).track_limit.max(PAGE_LIMIT);
        let tracks = ctx.db.list_tracks(track_limit, 0).await.unwrap_or_default();
        let albums = ctx.db.albums().await.unwrap_or_default();
        let artists = ctx.db.artists().await.unwrap_or_default();
        let most_played = ctx
            .db
            .most_played(MOST_PLAYED_LIMIT)
            .await
            .unwrap_or_default();
        let favourites = ctx.db.favourites(PAGE_LIMIT).await.unwrap_or_default();
        let history = ctx.db.history(HISTORY_LIMIT, 0).await.unwrap_or_default();
        let folders: Vec<FolderRow> = ctx
            .db
            .folders()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(id, path)| FolderRow {
                id: saturating_i32(id.max(0) as u64),
                path: path.to_string_lossy().to_string().into(),
            })
            .collect();

        let playlists = ctx.db.playlists().await.unwrap_or_default();

        // A scan, a rating or a favourite can change a page that is open, so reload
        // the one being looked at in the same pass instead of leaving it stale.
        let (section, open_album, open_artist, open_playlist) = {
            let lookup = lock(&lookup);
            (
                lookup.section.clone(),
                lookup.album.clone(),
                lookup.artist.clone(),
                lookup.playlist.clone(),
            )
        };

        let album_page = match (section.as_str() == "album", open_album) {
            (true, Some(page)) => match ctx.db.album_tracks(page.id).await {
                Ok(tracks) => {
                    remember_tracks(&lookup, &tracks);
                    let total_ms: i64 = tracks.iter().map(|track| track.duration_ms).sum();
                    let counts = format!("{} tracks · {}", tracks.len(), format_duration(total_ms));
                    let order: Vec<TrackId> = tracks.iter().map(|track| track.id).collect();
                    if let Some(open) = lock(&lookup).album.as_mut() {
                        open.order = order;
                    }
                    Some((track_rows(&tracks), counts))
                }
                Err(err) => {
                    log::error!("could not refresh the open album: {err}");
                    None
                }
            },
            _ => None,
        };

        let artist_page = match (section.as_str() == "artist", open_artist) {
            (true, Some(page)) => {
                let albums = ctx.db.artist_albums(page.id).await.unwrap_or_default();
                match ctx.db.artist_tracks(page.id).await {
                    Ok(tracks) => {
                        remember_tracks(&lookup, &tracks);
                        let total_ms: i64 = tracks.iter().map(|track| track.duration_ms).sum();
                        let meta = format!(
                            "{} albums · {} tracks · {}",
                            albums.len(),
                            tracks.len(),
                            format_duration(total_ms)
                        );
                        let order: Vec<TrackId> = tracks.iter().map(|track| track.id).collect();
                        if let Some(open) = lock(&lookup).artist.as_mut() {
                            open.order = order;
                        }
                        Some((album_rows(&albums), track_rows(&tracks), meta))
                    }
                    Err(err) => {
                        log::error!("could not refresh the open artist: {err}");
                        None
                    }
                }
            }
            _ => None,
        };

        let playlist_page = match (section.as_str() == "playlist", open_playlist) {
            (true, Some(page)) => match ctx.db.playlist_tracks(page.id).await {
                Ok(tracks) => {
                    remember_tracks(&lookup, &tracks);
                    let order: Vec<TrackId> = tracks.iter().map(|track| track.id).collect();
                    if let Some(open) = lock(&lookup).playlist.as_mut() {
                        open.order = order;
                    }
                    let summary = playlists.iter().find(|playlist| playlist.id == page.id);
                    let rules = ctx.db.playlist_rules(page.id).await.ok().flatten();
                    summary.map(|summary| PlaylistView::new(summary, &tracks, rules.as_ref()))
                }
                Err(err) => {
                    log::error!("could not refresh the open playlist: {err}");
                    None
                }
            },
            _ => None,
        };

        remember_tracks(&lookup, &tracks);
        remember_tracks(&lookup, &recent);
        remember_tracks(&lookup, &most_played);
        remember_tracks(&lookup, &favourites);
        remember_tracks(
            &lookup,
            &history
                .iter()
                .map(|entry| entry.track.clone())
                .collect::<Vec<_>>(),
        );
        let track_order = tracks.iter().map(|track| track.id).collect::<Vec<_>>();
        let lookup_for_ui = lookup.clone();
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
            if ui.get_search_text().is_empty() {
                lock(&lookup_for_ui).visible = track_order;
                ui.set_all_tracks(model(all_rows));
                ui.set_tracks_limit(saturating_i32(track_limit as u64));
            }
            ui.set_most_played_tracks(model(most_played_rows));
            ui.set_favourite_tracks(model(favourite_rows));
            ui.set_history_tracks(model(history_rows));
            ui.set_albums(model(album_rows));
            ui.set_artists(model(
                artists
                    .into_iter()
                    .map(|artist| ArtistRow {
                        id: saturating_i32(artist.id.max(0) as u64),
                        name: artist.name.into(),
                        meta: format!(
                            "{} albums · {} tracks",
                            artist.album_count, artist.track_count
                        )
                        .into(),
                    })
                    .collect(),
            ));
            ui.set_folders(model(folders));
            ui.set_playlists(model(playlist_rows(&playlists)));
            if let Some(view) = playlist_page {
                view.apply(&ui);
            }
            if let Some((rows, counts)) = album_page {
                ui.set_album_tracks(model(rows));
                ui.set_album_meta(counts.into());
            }
            if let Some((albums, rows, meta)) = artist_page {
                ui.set_artist_albums(model(albums));
                ui.set_artist_tracks(model(rows));
                ui.set_artist_meta(meta.into());
            }
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
        tokio::time::sleep(Duration::from_millis(120)).await;
        if generation.load(Ordering::SeqCst) != current {
            return;
        }
        let limit = lock(&lookup).track_limit.max(PAGE_LIMIT);
        let rows = if query.trim().is_empty() {
            ctx.db.list_tracks(limit, 0).await
        } else {
            ctx.db.search(&query, limit).await
        };

        let rows = match rows {
            Ok(rows) => rows,
            Err(err) => {
                set_status(&weak, format!("Search failed: {err}"));
                return;
            }
        };

        // A later keystroke already produced a newer query; drop this result.
        if generation.load(Ordering::SeqCst) != current {
            return;
        }

        let display = track_rows(&rows);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if generation.load(Ordering::SeqCst) != current
                || ui.get_search_text().as_str() != query
            {
                return;
            }
            remember(&lookup, &rows);
            ui.invoke_section_changed("tracks".into());
            ui.set_all_tracks(model(display));
            ui.set_tracks_limit(saturating_i32(limit as u64));
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

/// The same loader serves panel opens and playback changes, with one in-flight
/// request per track and a short negative/error cache for repeated opens.
#[derive(Clone, Default)]
struct LyricsLoader {
    timed: TimedLyrics,
    requests: Arc<Mutex<HashMap<TrackId, LyricsRequest>>>,
}

enum LyricsRequest {
    Loading,
    Complete { result: LyricsResult, at: Instant },
}

#[derive(Clone)]
struct LyricsResult {
    plain: String,
    synced: Option<String>,
    status: String,
    retry_after: Duration,
}

fn wire_lyrics(
    ui: &AppWindow,
    weak: Weak<AppWindow>,
    db: Db,
    handle: Handle,
    engine: Arc<Engine>,
    loader: LyricsLoader,
) {
    ui.on_lyrics_toggle(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let open = !window.get_lyrics_open();
        window.set_lyrics_open(open);
        if !open {
            return;
        }
        window.set_queue_open(false);
        match engine.current() {
            Some(track) => load_lyrics(&weak, &db, &handle, track, loader.clone()),
            None => {
                show_lyrics(&window, &loader.timed, "", None);
                window.set_lyrics_status("Nothing playing.".into());
            }
        }
    });
}

/// Cover art beside the transport controls.
///
/// Playback also moves on its own (gapless advance, the next button), so the
/// artwork watches the engine's current track instead of a click callback.
fn wire_artwork(weak: Weak<AppWindow>, db: Db, handle: Handle, engine: Arc<Engine>) -> Timer {
    let timer = Timer::default();
    let timer_weak = weak.clone();
    let mut shown: Option<(Option<TrackId>, Vec<TrackId>, AppearanceSettings)> = None;
    timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
        let Some(window) = timer_weak.upgrade() else {
            return;
        };

        let current = engine.current();
        if window.get_pending_track() != 0
            && current.map_or(0, |id| saturating_i32(id.max(0) as u64))
                != window.get_pending_track()
        {
            return;
        }
        let upcoming: Vec<TrackId> = engine.upcoming().into_iter().take(64).collect();
        let appearance = AppearanceSettings {
            blur_panels: window.get_settings_blur_panels(),
            cover_background: window.get_settings_cover_background(),
        };
        let selection = (current, upcoming.clone(), appearance);
        if shown.as_ref() == Some(&selection) {
            return;
        }
        shown = Some(selection);
        window.set_artwork_generation(window.get_artwork_generation().wrapping_add(1));
        let generation = window.get_artwork_generation();
        window.set_artwork_track(current.map_or(0, |track| saturating_i32(track.max(0) as u64)));

        match current {
            Some(track) => request_artwork(
                &weak,
                db.clone(),
                handle.clone(),
                track,
                upcoming,
                generation,
                appearance,
            ),
            None => window.set_artwork(slint::Image::default()),
        }
    });
    timer
}

/// Decode the track's embedded cover off the UI thread and hand it over.
fn request_artwork(
    weak: &Weak<AppWindow>,
    db: Db,
    handle: Handle,
    track: TrackId,
    upcoming: Vec<TrackId>,
    generation: i32,
    appearance: AppearanceSettings,
) {
    let weak = weak.clone();
    handle.spawn(async move {
        let Some(view) = db.track(track).await.ok().flatten() else {
            return;
        };
        let path = view.path.clone();
        // `slint::Image` is not `Send`, so the decode returns plain pixels and the
        // image is built on the UI thread.
        let pixels = tokio::task::spawn_blocking(move || {
            let cover = cover_pixels(&path, ARTWORK_PX);
            let (backdrop, panel_backdrop) = cover_backgrounds(cover.as_ref(), appearance);
            (cover, backdrop, panel_backdrop)
        })
        .await;
        let artwork_weak = weak.clone();
        let _ = artwork_weak.upgrade_in_event_loop(move |ui| {
            if ui.get_artwork_track() != saturating_i32(track.max(0) as u64)
                || ui.get_artwork_generation() != generation
            {
                return;
            }
            match pixels {
                Ok((cover, backdrop, panel_backdrop)) => {
                    ui.set_art_transition(false);
                    ui.set_previous_artwork(ui.get_artwork());
                    ui.set_previous_backdrop(ui.get_backdrop());
                    ui.set_previous_panel_backdrop(ui.get_panel_backdrop());
                    ui.set_artwork(pixel_image(cover));
                    if appearance.cover_background {
                        ui.set_backdrop(backdrop.map_or_else(slint::Image::default, |bytes| {
                            slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
                                &bytes,
                                BACKDROP_WIDTH,
                                BACKDROP_HEIGHT,
                            ))
                        }));
                    }
                    if appearance.cover_background && appearance.blur_panels {
                        ui.set_panel_backdrop(panel_backdrop.map_or_else(
                            slint::Image::default,
                            |bytes| {
                                slint::Image::from_rgba8(
                                    slint::SharedPixelBuffer::clone_from_slice(
                                        &bytes,
                                        BACKDROP_WIDTH,
                                        BACKDROP_HEIGHT,
                                    ),
                                )
                            },
                        ));
                    }
                    let weak = ui.as_weak();
                    slint::Timer::single_shot(Duration::ZERO, move || {
                        if let Some(ui) = weak.upgrade() {
                            if ui.get_artwork_generation() == generation {
                                ui.set_art_transition(true);
                            }
                        }
                    });
                }
                Err(err) => {
                    ui.set_status(format!("Artwork task failed: {err}").into());
                }
            }
        });
        // Queue thumbnails must not delay the new track's cover or backdrop.
        let mut queued_paths = Vec::new();
        for id in upcoming {
            match db.track(id).await {
                Ok(view) => queued_paths.push(view.map(|view| view.path)),
                Err(err) => {
                    log::warn!("could not read artwork track: {err}");
                    queued_paths.push(None);
                }
            }
        }
        let queued = tokio::task::spawn_blocking(move || {
            queued_paths
                .iter()
                .map(|path| {
                    path.as_deref()
                        .and_then(|path| cover_pixels(path, QUEUE_ARTWORK_PX))
                })
                .collect::<Vec<_>>()
        })
        .await;
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if ui.get_artwork_generation() != generation {
                return;
            }
            match queued {
                Ok(queued) => {
                    ui.set_queue_artworks(model(queued.into_iter().map(pixel_image).collect()))
                }
                Err(err) => log::warn!("queue artwork task failed: {err}"),
            }
        });
    });
}

fn cover_backgrounds(
    cover: Option<&CoverPixels>,
    appearance: AppearanceSettings,
) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    if !appearance.cover_background {
        return (None, None);
    }
    let Some((bytes, width, height)) = cover else {
        return (None, None);
    };
    let Some(cover) = image::RgbaImage::from_raw(*width, *height, bytes.clone()) else {
        return (None, None);
    };
    let mut canvas = image::DynamicImage::ImageRgba8(cover)
        .resize_to_fill(
            BACKDROP_WIDTH,
            BACKDROP_HEIGHT,
            image::imageops::FilterType::Triangle,
        )
        .to_rgba8();
    for pixel in canvas.pixels_mut() {
        let alpha = u32::from(pixel[3]);
        for channel in &mut pixel.0[..3] {
            *channel = ((u32::from(*channel) * alpha + 20 * (255 - alpha) + 127) / 255) as u8;
        }
        pixel[3] = 255;
    }
    // The Figma blur radii are rasterized at quarter resolution. Only the cover
    // is blurred; text and controls remain native, sharp Slint elements.
    let background = image::imageops::fast_blur(&canvas, 43.4375).into_raw();
    let panels = appearance
        .blur_panels
        .then(|| image::imageops::fast_blur(&canvas, 17.65).into_raw());
    (Some(background), panels)
}

fn pixel_image(pixels: Option<CoverPixels>) -> slint::Image {
    pixels.map_or_else(slint::Image::default, |(bytes, width, height)| {
        slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
            &bytes, width, height,
        ))
    })
}

/// Downscaled RGBA pixels of the file's embedded cover. A missing cover is
/// normal, so this returns `None` instead of an error.
fn cover_pixels(path: &Path, max_dimension: u32) -> Option<CoverPixels> {
    let raw = sonora_artwork::extract_embedded(path).ok().flatten()?;
    let decoded = image::load_from_memory(&raw).ok()?;
    let thumb = decoded.thumbnail(max_dimension, max_dimension).to_rgba8();
    Some((thumb.as_raw().clone(), thumb.width(), thumb.height()))
}

fn load_lyrics(
    weak: &Weak<AppWindow>,
    db: &Db,
    handle: &Handle,
    track: TrackId,
    loader: LyricsLoader,
) {
    if let Some(ui) = weak.upgrade() {
        ui.set_lyrics_track(saturating_i32(track.max(0) as u64));
        show_lyrics(&ui, &loader.timed, "", None);
        ui.set_lyrics_status("Loading lyrics…".into());
    }
    {
        let mut requests = lock(&loader.requests);
        if let Some(request) = requests.get(&track) {
            match request {
                LyricsRequest::Loading => return,
                LyricsRequest::Complete { result, at } if at.elapsed() < result.retry_after => {
                    display_lyrics(weak, track, &loader.timed, result.clone());
                    return;
                }
                _ => {}
            }
        }
        if requests.len() >= 128 {
            let oldest = requests
                .iter()
                .filter_map(|(id, request)| match request {
                    LyricsRequest::Complete { at, .. } => Some((*id, *at)),
                    LyricsRequest::Loading => None,
                })
                .min_by_key(|(_, at)| *at)
                .map(|(id, _)| id);
            if let Some(id) = oldest {
                requests.remove(&id);
            }
        }
        requests.insert(track, LyricsRequest::Loading);
    }
    let weak = weak.clone();
    let db = db.clone();
    handle.spawn(async move {
        let result = match lookup_lyrics(&db, track).await {
            Ok(result) => result,
            Err(err) => LyricsResult {
                plain: String::new(),
                synced: None,
                status: format!("Could not load lyrics: {err}. Reopen to retry in 30 seconds."),
                retry_after: Duration::from_secs(30),
            },
        };
        lock(&loader.requests).insert(
            track,
            LyricsRequest::Complete {
                result: result.clone(),
                at: Instant::now(),
            },
        );
        display_lyrics(&weak, track, &loader.timed, result);
    });
}

fn display_lyrics(
    weak: &Weak<AppWindow>,
    track: TrackId,
    timed: &TimedLyrics,
    result: LyricsResult,
) {
    let timed = timed.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        if !ui.get_lyrics_open() || ui.get_lyrics_track() != saturating_i32(track.max(0) as u64) {
            return;
        }
        show_lyrics(&ui, &timed, &result.plain, result.synced.as_deref());
        ui.set_lyrics_status(result.status.into());
    });
}

async fn lookup_lyrics(db: &Db, track: TrackId) -> anyhow::Result<LyricsResult> {
    let view = db
        .track(track)
        .await?
        .ok_or_else(|| anyhow::anyhow!("track is no longer in the library"))?;
    let path = view.path.clone();
    let local = tokio::task::spawn_blocking(move || local_lyrics(&path)).await??;
    let (plain, synced, source) = if let Some((plain, synced)) = local {
        (plain, synced, "local")
    } else if let Some(stored) = db.lyrics_for(track).await? {
        return Ok(LyricsResult {
            plain: lyrics_text(stored.plain.as_deref(), stored.synced.as_deref()),
            synced: stored.synced,
            status: format!("Cached ({})", stored.source),
            retry_after: Duration::from_secs(300),
        });
    } else {
        let query = sonora_metadata::TrackQuery {
            artist: view.artist.unwrap_or_else(|| "Unknown Artist".to_string()),
            title: view.title,
            album: view.album,
            duration_ms: (view.duration_ms > 0).then_some(view.duration_ms as u64),
        };
        let lyrics = tokio::task::spawn_blocking(move || {
            let http = sonora_metadata::UreqHttp::new(sonora_metadata::USER_AGENT);
            sonora_metadata::fetch_lyrics(&http, &query)
        })
        .await??;
        match lyrics {
            Some(lyrics) => (lyrics.plain, lyrics.synced, "lrclib"),
            None => {
                return Ok(LyricsResult {
                    plain: String::new(),
                    synced: None,
                    status: "No lyrics available for this track.".to_string(),
                    retry_after: Duration::from_secs(300),
                })
            }
        }
    };
    let mut status = if synced.is_some() {
        format!("Synced lyrics ({source})")
    } else {
        format!("Lyrics ({source})")
    };
    if let Err(err) = db
        .save_lyrics(track, plain.as_deref(), synced.as_deref(), source)
        .await
    {
        status.push_str(&format!(" · Could not cache: {err}"));
    }
    Ok(LyricsResult {
        plain: lyrics_text(plain.as_deref(), synced.as_deref()),
        synced,
        status,
        retry_after: Duration::from_secs(300),
    })
}

/// Read a local sibling `.lrc` before asking a remote provider.
fn local_lyrics(path: &Path) -> std::io::Result<Option<(Option<String>, Option<String>)>> {
    let candidate = path.with_extension("lrc");
    let raw = match std::fs::read_to_string(&candidate) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let synced = if !parse_lrc_timed(trimmed).is_empty() {
        Some(trimmed.to_string())
    } else {
        None
    };
    let plain = if synced.is_some() {
        let text = lyrics_text(None, Some(trimmed));
        (!text.is_empty()).then_some(text)
    } else {
        Some(trimmed.to_string())
    };
    if plain.is_none() && synced.is_none() {
        return Ok(None);
    }
    Ok(Some((plain, synced)))
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

/// Timed lines for the track in the pane, shared between the loader (a Tokio
/// task that replaces them) and the playback poll (which reads them to move the
/// highlight). A new track replaces the whole vector.
type TimedLyrics = Arc<Mutex<Vec<(u64, String)>>>;

/// Hand loaded lyrics to the pane. Synced payloads are split into timed lines
/// the player follows; plain payloads stay one block of text.
fn show_lyrics(ui: &AppWindow, timed: &TimedLyrics, text: &str, synced: Option<&str>) {
    let lines = synced.map(parse_lrc_timed).unwrap_or_default();
    ui.set_lyric_lines(lyric_model(&lines));
    ui.set_lyric_active(-1);
    ui.set_lyrics_text(text.into());
    *lock(timed) = lines;
}

fn lyric_model(lines: &[(u64, String)]) -> ModelRc<SharedString> {
    let rows: Vec<SharedString> = lines
        .iter()
        .map(|(_, text)| SharedString::from(text.as_str()))
        .collect();
    ModelRc::from(std::rc::Rc::new(VecModel::from(rows)))
}

/// Timed lines from a synced LRC payload, in playback order. A line with
/// several timestamps becomes one entry per timestamp; metadata-only lines are
/// ignored. The moments are in milliseconds from the start of the track.
fn parse_lrc_timed(synced: &str) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for line in synced.lines() {
        let mut rest = line.trim_start();
        let mut times = Vec::new();
        while rest.starts_with('[') {
            match rest.find(']') {
                Some(end) => {
                    if let Some(ms) = parse_lrc_timestamp(&rest[1..end]) {
                        times.push(ms);
                    }
                    rest = rest[end + 1..].trim_start();
                }
                None => break,
            }
        }
        if times.is_empty() {
            continue;
        }
        let text = rest.to_string();
        for ms in times {
            out.push((ms, text.clone()));
        }
    }
    out.sort_by_key(|(ms, _)| *ms);
    out
}

fn parse_lrc_timestamp(tag: &str) -> Option<u64> {
    let (mm, rest) = tag.split_once(':')?;
    if mm.is_empty() || !mm.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let minutes: u64 = mm.parse().ok()?;
    let (ss, frac_ms) = if let Some((s, f)) = rest.split_once('.') {
        let sec: u64 = if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
            s.parse().ok()?
        } else {
            return None;
        };
        let digits: String = f.chars().take_while(|c| c.is_ascii_digit()).collect();
        let frac = match digits.len() {
            0 => 0,
            1 => digits.parse::<u64>().ok()? * 100,
            2 => digits.parse::<u64>().ok()? * 10,
            _ => digits[..3].parse::<u64>().ok()?,
        };
        (sec, frac)
    } else {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        (digits.parse().ok()?, 0)
    };
    Some(minutes * 60_000 + ss * 1000 + frac_ms)
}

/// Index of the timed line that is active at `position_ms`, or -1 when the
/// playback position is before the first timed line.
fn current_lyric_index(lines: &[(u64, String)], position_ms: u64) -> i32 {
    let next = lines.partition_point(|(ms, _)| *ms <= position_ms);
    if next == 0 {
        -1
    } else {
        (next - 1) as i32
    }
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
    ui.set_settings_history_retention(settings.history_retention.index() as i32);
    ui.set_settings_blur_panels(settings.appearance.blur_panels);
    ui.set_settings_cover_background(settings.appearance.cover_background);
    ui.set_settings_device(settings.playback_device.as_deref().unwrap_or("").into());
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
    let now = now_playing(engine, lookup);
    let current = engine
        .current()
        .map_or(0, |track| saturating_i32(track.max(0) as u64));

    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.set_queue_entries(model(entries));
        if ui.get_pending_track() != 0 && ui.get_pending_track() != current {
            return;
        }
        ui.set_pending_track(0);
        apply_now_playing(&ui, &now);
    });

    refresh_queue_tabs(weak, engine);
}
fn apply_now_playing(ui: &AppWindow, now: &TrackInfo) {
    ui.set_now_playing_title(now.title.clone().into());
    ui.set_now_playing_subtitle(now.subtitle.clone().into());
    ui.set_now_playing_artist(now.artist.clone().into());
    ui.set_now_playing_artist_id(saturating_i32(now.artist_id.max(0) as u64));
    ui.set_now_playing_album_id(saturating_i32(now.album_id.max(0) as u64));
}

fn now_playing(engine: &Engine, lookup: &Arc<Mutex<TrackLookup>>) -> TrackInfo {
    let current = engine.current();
    let lookup = lock(lookup);
    current
        .and_then(|track| lookup.labels.get(&track).cloned())
        .unwrap_or_else(|| TrackInfo {
            title: "Nothing playing".to_string(),
            ..TrackInfo::default()
        })
}
fn queue_rows(engine: &Engine, lookup: &Arc<Mutex<TrackLookup>>) -> Vec<QueueRow> {
    let upcoming = engine.upcoming();
    let lookup = lock(lookup);
    upcoming
        .into_iter()
        .map(|track| {
            let info = lookup
                .labels
                .get(&track)
                .cloned()
                .unwrap_or_else(|| TrackInfo {
                    title: format!("Track {track}"),
                    ..TrackInfo::default()
                });
            QueueRow {
                title: info.title.into(),
                artist: info.artist.clone().into(),
                artist_id: saturating_i32(info.artist_id.max(0) as u64),
                album_id: saturating_i32(info.album_id.max(0) as u64),
                subtitle: info.subtitle.into(),
            }
        })
        .collect()
}
fn remember(lookup: &Arc<Mutex<TrackLookup>>, tracks: &[TrackView]) {
    remember_tracks(lookup, tracks);
    lock(lookup).visible = tracks.iter().map(|track| track.id).collect();
}

/// Record what playback and the queue need for tracks that are on screen,
/// without claiming they are "the list on screen": the album page keeps its own
/// order, and leaving it must not change how other pages queue.
fn remember_tracks(lookup: &Arc<Mutex<TrackLookup>>, tracks: &[TrackView]) {
    let mut lookup = lock(lookup);
    for track in tracks {
        lookup.paths.insert(track.id, track.path.clone());
        lookup.labels.insert(
            track.id,
            TrackInfo {
                title: track.title.clone(),
                subtitle: subtitle_for(track),
                artist: track.artist.clone().unwrap_or_default(),
                artist_id: track.artist_id.unwrap_or(0),
                album_id: track.album_id.unwrap_or(0),
            },
        );
        lookup.gains.insert(
            track.id,
            track.effective_replay_gain_db().map(|db| db as f32),
        );
    }
}
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

fn playlist_rows(playlists: &[PlaylistSummary]) -> Vec<PlaylistRow> {
    playlists
        .iter()
        .map(|playlist| PlaylistRow {
            id: saturating_i32(playlist.id as u64),
            name: playlist.name.clone().into(),
            count: saturating_i32(playlist.track_count as u64),
            smart: playlist.smart,
        })
        .collect()
}

fn row_cover_key(track: &TrackView) -> String {
    format!("{}:{}:{}", track.id, track.path.display(), track.file_size)
}

fn track_rows(tracks: &[TrackView]) -> Vec<TrackRow> {
    tracks
        .iter()
        .map(|track| TrackRow {
            id: saturating_i32(track.id as u64),
            cover_key: row_cover_key(track).into(),
            title: track.title.clone().into(),
            artist: track.artist.clone().unwrap_or_default().into(),
            artist_id: saturating_i32(track.artist_id.unwrap_or(0).max(0) as u64),
            album: track.album.clone().unwrap_or_default().into(),
            album_id: saturating_i32(track.album_id.unwrap_or(0).max(0) as u64),
            subtitle: String::new().into(),
            duration: format_duration(track.duration_ms).into(),
            favourite: track.favourite,
            rating: track.rating.clamp(0, 5) as i32,
            replay_gain: track
                .effective_replay_gain_db()
                .map(|db| format!("{:+0.1} dB", db))
                .unwrap_or_default()
                .into(),
        })
        .collect()
}
fn history_rows(entries: &[HistoryEntry]) -> Vec<TrackRow> {
    entries
        .iter()
        .map(|entry| {
            let mut extra = relative_time(entry.played_at);
            if entry.skipped {
                extra.push_str(" · skipped");
            }
            TrackRow {
                id: saturating_i32(entry.track.id as u64),
                cover_key: row_cover_key(&entry.track).into(),
                title: entry.track.title.clone().into(),
                artist: entry.track.artist.clone().unwrap_or_default().into(),
                artist_id: saturating_i32(entry.track.artist_id.unwrap_or(0).max(0) as u64),
                album: entry.track.album.clone().unwrap_or_default().into(),
                album_id: saturating_i32(entry.track.album_id.unwrap_or(0).max(0) as u64),
                subtitle: extra.into(),
                duration: format_duration(entry.track.duration_ms).into(),
                favourite: entry.track.favourite,
                rating: entry.track.rating.clamp(0, 5) as i32,
                replay_gain: String::new().into(),
            }
        })
        .collect()
}
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
                id: saturating_i32(album.id as u64),
                title: album.title.clone().into(),
                subtitle: album.album_artist.clone().into(),
                artist_id: saturating_i32(album.album_artist_id.max(0) as u64),
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
    fn local_lyrics_reads_a_sibling_lrc_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let track = dir.path().join("song.flac");
        std::fs::write(&track, b"audio").unwrap();
        std::fs::write(
            dir.path().join("song.lrc"),
            "[00:01.00] first line\n[00:02.00] second line\n",
        )
        .unwrap();
        let (plain, synced) = local_lyrics(&track).unwrap().expect("local lyrics");
        assert!(synced.as_deref().unwrap().contains("[00:01.00]"));
        assert_eq!(plain.as_deref(), Some("first line\nsecond line"));
    }

    #[test]
    fn local_lyrics_treats_untimed_text_as_plain() {
        let dir = tempfile::tempdir().expect("temp dir");
        let track = dir.path().join("song.mp3");
        std::fs::write(&track, b"audio").unwrap();
        std::fs::write(dir.path().join("song.lrc"), "hello\nworld\n").unwrap();
        let (plain, synced) = local_lyrics(&track).unwrap().expect("local lyrics");
        assert_eq!(plain.as_deref(), Some("hello\nworld"));
        assert_eq!(synced, None);
    }

    #[test]
    fn local_lyrics_ignores_a_missing_lrc_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let track = dir.path().join("song.ogg");
        std::fs::write(&track, b"audio").unwrap();
        assert!(local_lyrics(&track).unwrap().is_none());
    }

    #[test]
    fn cover_effects_generate_blurred_pixels_and_respect_both_options() {
        let image = image::RgbaImage::from_fn(64, 64, |x, _| {
            if x < 32 {
                image::Rgba([240, 10, 10, 255])
            } else {
                image::Rgba([10, 10, 240, 255])
            }
        });
        let cover = (image.into_raw(), 64, 64);
        let (background, panels) = cover_backgrounds(Some(&cover), AppearanceSettings::default());
        let expected = (BACKDROP_WIDTH * BACKDROP_HEIGHT * 4) as usize;
        assert_eq!(background.as_ref().unwrap().len(), expected);
        assert_eq!(panels.as_ref().unwrap().len(), expected);
        assert_ne!(
            background, panels,
            "panel and scene blur have distinct measured radii"
        );
        let (background, panels) = cover_backgrounds(
            Some(&cover),
            AppearanceSettings {
                blur_panels: false,
                cover_background: true,
            },
        );
        assert!(background.is_some());
        assert!(panels.is_none());
        assert_eq!(
            cover_backgrounds(
                Some(&cover),
                AppearanceSettings {
                    blur_panels: true,
                    cover_background: false
                }
            ),
            (None, None)
        );
        assert_eq!(
            cover_backgrounds(None, AppearanceSettings::default()),
            (None, None)
        );
    }

    #[test]
    fn bracketed_plain_lyrics_stay_plain_and_unreadable_local_files_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let track = dir.path().join("track.flac");
        let local = track.with_extension("lrc");
        std::fs::write(&local, "[Chorus]\nSing along").unwrap();
        let (plain, synced) = local_lyrics(&track).unwrap().unwrap();
        assert_eq!(plain.as_deref(), Some("[Chorus]\nSing along"));
        assert!(synced.is_none());
        std::fs::remove_file(&local).unwrap();
        std::fs::create_dir(&local).unwrap();
        assert!(local_lyrics(&track).is_err());
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

#[cfg(test)]
mod timed_lyrics {
    use super::{current_lyric_index, parse_lrc_timed, parse_lrc_timestamp};

    #[test]
    fn current_lyric_index_returns_negative_one_before_first_line() {
        let lines = vec![(1_000, "first".into()), (2_000, "second".into())];
        assert_eq!(current_lyric_index(&lines, 500), -1);
        assert_eq!(current_lyric_index(&lines, 0), -1);
    }

    #[test]
    fn current_lyric_index_selects_by_position() {
        let lines = vec![
            (1_000, "first".into()),
            (2_000, "second".into()),
            (3_000, "third".into()),
        ];
        assert_eq!(current_lyric_index(&lines, 1_000), 0);
        assert_eq!(current_lyric_index(&lines, 1_500), 0);
        assert_eq!(current_lyric_index(&lines, 1_999), 0);
        assert_eq!(current_lyric_index(&lines, 2_000), 1);
        assert_eq!(current_lyric_index(&lines, 2_999), 1);
        assert_eq!(current_lyric_index(&lines, 3_000), 2);
        assert_eq!(current_lyric_index(&lines, 9_999_999), 2,);
    }

    #[test]
    fn parse_lrc_timed_orders_and_highlights_by_position() {
        let input = concat!(
            "[00:10.00] first line\n",
            "[00:20.00] second line\n",
            "[00:30.00] third line\n"
        );
        let lines = parse_lrc_timed(input);
        assert_eq!(
            lines,
            vec![
                (10_000, "first line".into()),
                (20_000, "second line".into()),
                (30_000, "third line".into()),
            ]
        );
        assert_eq!(current_lyric_index(&lines, 14_999), 0,);
        assert_eq!(current_lyric_index(&lines, 25_000), 1,);
    }

    #[test]
    fn parse_lrc_timed_orders_out_of_sequence_timestamps() {
        let input = concat!(
            "[00:30.00] late line\n",
            "[00:05.00] early line\n",
            "[00:15.00] middle line\n"
        );
        let lines = parse_lrc_timed(input);
        assert_eq!(
            lines,
            vec![
                (5_000, "early line".into()),
                (15_000, "middle line".into()),
                (30_000, "late line".into()),
            ]
        );
    }

    #[test]
    fn parse_lrc_timed_rejects_broken_timestamps_and_keeps_readable_lines() {
        let input = concat!(
            "[00:10.00] kept\n",
            "[bad] ignored\n",
            "[00:20.00] also kept\n",
            ":10.00\n",
            "meta\n"
        );
        let lines = parse_lrc_timed(input);
        assert_eq!(
            lines,
            vec![(10_000, "kept".into()), (20_000, "also kept".into()),]
        );
    }

    #[test]
    fn parse_lrc_timestamp_handles_millisecond_precision() {
        assert_eq!(parse_lrc_timestamp("00:01.01"), Some(1_010),);
        assert_eq!(parse_lrc_timestamp("00:01.1"), Some(1_100),);
        assert_eq!(parse_lrc_timestamp("00:01.12345"), Some(1_123),);
    }

    #[test]
    fn parse_lrc_timestamp_handles_crlf_lrc() {
        let input = "[00:10.00] first\r\n[00:20.00] second\r\n";
        let lines = parse_lrc_timed(input);
        assert_eq!(
            lines,
            vec![(10_000, "first".into()), (20_000, "second".into()),]
        );
    }
}

#[cfg(test)]
mod interaction_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};

    struct TestPlatform(Rc<MinimalSoftwareWindow>);

    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    fn click(ui: &AppWindow, x: f32, y: f32) {
        let position = slint::LogicalPosition::new(x, y);
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
    }

    #[test]
    fn pointer_controls_scrub_continuously_and_panels_keep_their_geometry() {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        slint::platform::set_platform(Box::new(TestPlatform(window.clone()))).unwrap();
        let ui = AppWindow::new().unwrap();
        window.set_size(slint::PhysicalSize::new(1920, 1080));
        ui.set_playback_available(true);
        ui.set_seekable(true);
        ui.set_duration_ms(100_000);
        ui.set_queue_open(false);
        let toggles = Rc::new(Cell::new(0));
        let toggles_for_callback = toggles.clone();
        ui.on_toggle_playback(move || toggles_for_callback.set(toggles_for_callback.get() + 1));
        let seeks = Rc::new(RefCell::new(Vec::new()));
        let seeks_for_callback = seeks.clone();
        ui.on_seek_requested(move |value| seeks_for_callback.borrow_mut().push(value));
        let volume = Rc::new(Cell::new(-1.0));
        let volume_for_callback = volume.clone();
        ui.on_volume_changed(move |value| volume_for_callback.set(value));
        ui.show().unwrap();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1920 * 1080];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1920);
        });

        click(&ui, 142.0, 422.0);
        assert_eq!(
            toggles.get(),
            1,
            "the focus scope must not intercept pointer clicks"
        );
        click(&ui, 85.0, 542.0);
        assert!(
            (volume.get() - 0.25).abs() < 0.05,
            "volume click did not reach the slider"
        );
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position: slint::LogicalPosition::new(51.0, 388.0),
            button: PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(143.0, 388.0),
        });
        assert!(ui.get_seeking());
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1920);
        });
        assert!(
            pixels[388 * 1920 + 60].r > 210,
            "progress must fill from its left edge"
        );
        assert!(
            pixels[388 * 1920 + 230].r < 210,
            "the unplayed right half must stay unfilled"
        );

        ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(900.0, 388.0),
        });
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position: slint::LogicalPosition::new(900.0, 388.0),
            button: PointerEventButton::Left,
        });
        assert!(!ui.get_seeking());
        let values = seeks.borrow();
        assert!(values.len() >= 3, "dragging must continuously send seeks");
        assert!((values[0] - 10_000).abs() < 500);
        assert!(values.iter().any(|value| (*value - 50_000).abs() < 500));
        assert_eq!(
            values.last(),
            Some(&100_000),
            "captured release outside the bar must clamp"
        );
        drop(values);

        ui.set_section("settings".into());
        ui.set_settings_format_tier("lossy".into());
        ui.set_folders(model(vec![FolderRow {
            id: 1,
            path: "/generated/music".into(),
        }]));
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1920);
        });
        // The cache and bitrate rails must occupy most of the page, not collapse
        // to the sliders' intrinsic minimum width.
        let white_on_row = |y: usize| {
            pixels[y * 1920..(y + 1) * 1920]
                .iter()
                .filter(|pixel| pixel.r > 210 && pixel.g > 210 && pixel.b > 210)
                .count()
        };
        assert!(
            (300..1000).any(|y| white_on_row(y) > 150),
            "no full-width settings slider was rendered"
        );
        let panel = slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
            &[240u8, 0, 0, 255],
            1,
            1,
        ));
        ui.set_panel_backdrop(panel);
        ui.set_settings_cover_background(true);
        ui.set_settings_blur_panels(true);
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1920);
        });
        let blurred = pixels[60 * 1920 + 900];
        assert!(blurred.r > blurred.g + 20);
        ui.set_settings_blur_panels(false);
        std::thread::sleep(Duration::from_millis(170));
        slint::platform::update_timers_and_animations();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1920);
        });
        let plain = pixels[60 * 1920 + 900];
        assert_eq!(
            plain.r, plain.g,
            "disabling panel blur must remove the cover layer"
        );
        ui.set_lyrics_open(true);
        ui.set_lyric_lines(model(
            (0..150)
                .map(|index| format!("Line {index}: a generated lyric").into())
                .collect(),
        ));
        ui.set_lyric_active(60);
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1920);
        });
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1920);
        });
        let lyric_pixels = (300..950)
            .flat_map(|y| (1560..1880).map(move |x| (x, y)))
            .filter(|(x, y)| {
                let pixel = pixels[y * 1920 + x];
                pixel.r > 170 && pixel.g > 170 && pixel.b > 170
            })
            .count();
        assert!(
            lyric_pixels > 500,
            "the lyrics viewport collapsed instead of showing its content"
        );
        ui.window().dispatch_event(WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(1750.0, 600.0),
            delta_x: 0.0,
            delta_y: -200.0,
        });
        assert!(
            !ui.get_lyrics_following(),
            "wheel scrolling must suspend automatic following"
        );
        ui.set_lyric_active(61);
        assert!(!ui.get_lyrics_following());

        ui.set_lyrics_open(false);
        ui.set_queue_open(true);
        ui.set_section("tracks".into());
        ui.set_all_tracks(model(
            (0..600)
                .map(|index| TrackRow {
                    id: index,
                    title: format!("Track {index} {}", "long title ".repeat(30)).into(),
                    artist: "long artist name ".repeat(30).into(),
                    album: "long album name ".repeat(30).into(),
                    duration: "10:00".into(),
                    ..TrackRow::default()
                })
                .collect(),
        ));
        window.set_size(slint::PhysicalSize::new(920, 600));
        pixels.resize(920 * 600, slint::Rgb8Pixel::default());
        assert!(window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 920);
        }));
        let selected = Rc::new(Cell::new(-1));
        let selected_for_callback = selected.clone();
        ui.on_track_activated(move |id| selected_for_callback.set(id));
        click(&ui, 300.0, 135.0);
        assert_eq!(
            selected.get(),
            0,
            "the song title must activate its full row"
        );
        selected.set(-1);
        click(&ui, 260.0, 140.0);
        assert_eq!(
            selected.get(),
            0,
            "cover artwork must remain part of the play hit target"
        );
        ui.hide().unwrap();
    }
}
