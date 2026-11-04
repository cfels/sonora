//! Sonora entry point.
//!
//! Two modes for now:
//!   sonora                 — desktop UI (Windows, Linux)
//!   sonora --scan <folder> — headless index, useful in scripts and CI
//!
//! The database always lives in the platform data directory, so UI and CLI
//! share one library. Android has no such directory to discover, so its entry
//! point supplies one through `SONORA_DATA_DIR`.

use std::path::PathBuf;

use anyhow::{bail, Context};
use sonora_db::Db;
use sonora_scan::Scanner;

const USAGE: &str = "\
Sonora — a music library for people who own their files

USAGE:
    sonora                      Open the desktop application
    sonora --scan <folder>      Index a folder without opening the UI
    sonora --artwork <folder>   Export cover art for every track in a folder
    sonora --lyrics <query>     Fetch and store lyrics for the best search match
    sonora --cover <query>      Fetch and store cover art for the best search match
    sonora --where              Print the library database path
    sonora --settings           Print the current settings as JSON
    sonora --version            Print the version

ENVIRONMENT:
    SONORA_DATA_DIR             Override the data directory (tests, portable installs)
    RUST_LOG                    Log filter, e.g. RUST_LOG=debug
";

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .context("failed to start the async runtime")?;

    let db_path = database_path()?;
    let db = runtime.block_on(Db::open(&db_path)).with_context(|| {
        format!(
            "could not open the library database at {}",
            db_path.display()
        )
    })?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {
            log::info!("library database: {}", db_path.display());
            sonora_ui::run(db, runtime.handle().clone())?;
        }
        [flag] if flag == "--version" || flag == "-V" => {
            println!("sonora {}", env!("CARGO_PKG_VERSION"));
        }
        [flag] if flag == "--where" => {
            println!("{}", db_path.display());
        }
        [flag] if flag == "--settings" => {
            let settings = runtime.block_on(db.load_settings())?;
            println!("{}", settings.to_json_pretty());
        }
        [flag, folder] if flag == "--artwork" => {
            export_artwork(&runtime, &db, folder)?;
        }
        [flag, query] if flag == "--lyrics" => {
            show_lyrics(&runtime, &db, query)?;
        }
        [flag, query] if flag == "--cover" => {
            save_cover(&runtime, &db, &db_path, query)?;
        }
        [flag] if flag == "--help" || flag == "-h" => {
            println!("{USAGE}");
        }
        [flag, folder] if flag == "--scan" => {
            let scanner = Scanner::new(db.clone());
            let report = runtime
                .block_on(scanner.scan(folder, None))
                .context("scan failed")?;
            println!("Library:   {}", db_path.display());
            println!("Folder:    {}", report.folder.display());
            println!("Scanned:   {}", report.seen);
            println!("Imported:  {}", report.imported);
            println!("Updated:   {}", report.updated);
            println!("Unchanged: {}", report.unchanged);
            println!("Removed:   {}", report.removed);
            println!("Errors:    {}", report.total_errors());
            for error in report.errors.iter().take(10) {
                eprintln!("  {}: {}", error.path.display(), error.message);
            }
            println!("Elapsed:   {} ms", report.elapsed_ms);
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }

    Ok(())
}

/// Extract embedded covers, convert them to the user's format and write them
/// beside the audio files. Refuses to replace a cover that is already there.
fn export_artwork(runtime: &tokio::runtime::Runtime, db: &Db, folder: &str) -> anyhow::Result<()> {
    let folder = PathBuf::from(folder);
    let folder = folder.canonicalize().unwrap_or(folder);
    if !folder.is_dir() {
        bail!("{} is not a folder Sonora can read", folder.display());
    }

    let settings = runtime.block_on(db.load_settings())?;
    let options = sonora_artwork::ArtworkOptions {
        format: settings.artwork.format,
        quality: settings.artwork.quality,
        max_dimension: sonora_artwork::DEFAULT_MAX_DIMENSION,
    };

    let mut paths = Vec::new();
    let mut offset = 0i64;
    loop {
        let page = runtime.block_on(db.list_tracks(500, offset))?;
        if page.is_empty() {
            break;
        }
        offset += page.len() as i64;
        paths.extend(
            page.into_iter()
                .map(|track| track.path)
                .filter(|path| path.starts_with(&folder)),
        );
    }

    let report = sonora_artwork::process_paths(paths, options, false);

    println!("Folder:    {}", folder.display());
    println!("Format:    {}", settings.artwork.format.label());
    println!("Tracks:    {}", report.tracks);
    println!("Written:   {}", report.written);
    println!("Unchanged: {}", report.unchanged);
    println!("Kept:      {}", report.kept_existing);
    println!("No art:    {}", report.without_artwork);
    println!("Failed:    {}", report.failed);
    Ok(())
}

/// Fetch lyrics for the track that best matches `query` and keep them.
///
/// Lyrics already in the database are printed without another request, so
/// repeated lookups cost nothing.
fn show_lyrics(runtime: &tokio::runtime::Runtime, db: &Db, query: &str) -> anyhow::Result<()> {
    let track = best_match(runtime, db, query)?;
    let wanted = track_query(&track);

    if let Some(stored) = runtime.block_on(db.lyrics_for(track.id))? {
        println!("Track:  {}", label(&track));
        println!("Source: {} (cached)", stored.source);
        if let Some(text) = stored.display_text() {
            println!();
            println!("{text}");
        }
        return Ok(());
    }

    let http = sonora_metadata::UreqHttp::new(sonora_metadata::USER_AGENT);
    match sonora_metadata::fetch_lyrics(&http, &wanted)? {
        Some(lyrics) => {
            runtime.block_on(db.save_lyrics(
                track.id,
                lyrics.plain.as_deref(),
                lyrics.synced.as_deref(),
                "lrclib",
            ))?;
            println!("Track:  {}", label(&track));
            println!(
                "Source: LRCLIB ({})",
                if lyrics.synced.is_some() {
                    "synced"
                } else {
                    "plain"
                }
            );
            if let Some(text) = lyrics.display_text() {
                println!();
                println!("{text}");
            }
        }
        None => println!("No lyrics found for {}.", label(&track)),
    }
    Ok(())
}

/// Fetch a cover for the album of the best match and put it in the artwork cache.
fn save_cover(
    runtime: &tokio::runtime::Runtime,
    db: &Db,
    db_path: &std::path::Path,
    query: &str,
) -> anyhow::Result<()> {
    let track = best_match(runtime, db, query)?;
    let wanted = track_query(&track);
    let http = sonora_metadata::UreqHttp::new(sonora_metadata::USER_AGENT);

    let Some(cover) = sonora_metadata::find_cover(&http, &wanted)? else {
        println!("No cover found for {}.", label(&track));
        return Ok(());
    };

    let settings = runtime.block_on(db.load_settings())?;
    let root = db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("artwork");
    let store = sonora_artwork::ArtworkStore::at(root);
    let stored = store.store(
        &cover.bytes,
        sonora_artwork::ArtworkOptions {
            format: settings.artwork.format,
            quality: settings.artwork.quality,
            max_dimension: sonora_artwork::DEFAULT_MAX_DIMENSION,
        },
    )?;

    println!("Track:  {}", label(&track));
    println!("Source: {} ({})", cover.source.label(), cover.url);
    println!("Saved:  {}", stored.path.display());
    println!("Bytes:  {}", stored.bytes);
    Ok(())
}

/// The first search hit, so the CLI stays deterministic instead of guessing.
fn best_match(
    runtime: &tokio::runtime::Runtime,
    db: &Db,
    query: &str,
) -> anyhow::Result<sonora_core::TrackView> {
    let mut hits = runtime.block_on(db.search(query, 1))?;
    hits.pop()
        .with_context(|| format!("no track in the library matches \"{query}\""))
}

fn track_query(track: &sonora_core::TrackView) -> sonora_metadata::TrackQuery {
    sonora_metadata::TrackQuery {
        artist: track
            .artist
            .clone()
            .unwrap_or_else(|| "Unknown Artist".to_string()),
        title: track.title.clone(),
        album: track.album.clone(),
        duration_ms: Some(track.duration_ms.max(0) as u64),
    }
}

fn label(track: &sonora_core::TrackView) -> String {
    match track.artist.as_deref() {
        Some(artist) => format!("{artist} — {}", track.title),
        None => track.title.clone(),
    }
}

fn database_path() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("SONORA_DATA_DIR") {
        return Ok(PathBuf::from(dir).join("library.db"));
    }

    let Some(project) = directories::ProjectDirs::from("dev", "sonora", "Sonora") else {
        bail!("could not determine a data directory; set SONORA_DATA_DIR (required on Android)");
    };
    Ok(project.data_dir().join("library.db"))
}
