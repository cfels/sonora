# Sonora architecture

Status: phase 1 complete; phase 2 (playback, transport, queue) and the first
slices of phases 3 and 4 (artwork, keyless metadata, lyrics) are implemented.
Sections marked *(planned)* describe modules that are designed but not yet
written.

## Guiding constraint

Sonora is a native application for Windows, Linux and Android. The UI is compiled
Slint markup rendered by a native backend; there is no web view, no bundled
Chromium and no JavaScript runtime. Everything else — scanning, playback, network calls, transfers — is
Rust in the same process, coordinated through typed messages rather than shared
mutable state.

## Platform targets

| Target | Status | Notes |
|---|---|---|
| Windows 10/11 (x64, arm64) | primary | MSVC build; `setupapi` for device detection |
| Linux (x64, arm64) | primary | X11/Wayland; udev and mount table for devices |
| Android (arm64-v8a; x86_64 for emulators) | first-class, in progress | touch layout, SAF storage, foreground playback service; no device sync |

Android is not a port bolted on at the end; it constrains the design from here
on:

- **The UI is responsive, not desktop-shaped.** The fixed sidebar and wide
  multi-column rows become a bottom navigation bar and single-column lists below
  a width breakpoint; the queue becomes a sheet instead of a side panel. Hover
  is never the only affordance for an action.
- **Folder access goes through the platform picker.** Android has no equivalent
  of "scan this arbitrary directory": folder selection uses the Storage Access
  Framework and the scanner consumes document URIs through readable file
  descriptors rather than `walkdir`-ing a path. Desktop keeps plain paths, so the
  scanner exposes both sources behind one interface.
- **The data directory is supplied, not discovered.** There is no `$HOME` on
  Android, so `ProjectDirs` cannot resolve a library location. The Android entry
  point passes the app's files directory — `SONORA_DATA_DIR` during development,
  the activity context in the real harness.
- **Playback lives in a foreground service.** A media session, notification and
  wake lock keep the queue alive when the activity is backgrounded; the same
  GStreamer engine backs both platforms.
- **Media controls and presence are per-platform, not desktop-only.** System
  media controls mean MPRIS on Linux, SMTC on Windows, and a `MediaSession` plus
  media notification on Android. Discord Rich Presence also works on Android:
  the Discord Social SDK (1.10+) publishes presence over RPC without account
  linking when the Discord app is installed and signed in, so the Android build
  keeps the feature instead of hiding it. External device sync stays Windows and
  Linux only by design — a phone is a playback client, never a sync host — so
  the Android build excludes the device crate instead of exposing a control that
  cannot work.

GStreamer on Android is the biggest open risk: it ships as a separate build of
prebuilt shared objects with a different plugin set, and it must be reproduced
for each ABI we package.

## Crate layout

```
sonora/
├── assets/branding/          logo mark used by the UI and packaging
├── crates/
│   ├── sonora-core/          domain types, errors, engine→UI events, helpers
│   ├── sonora-db/            SQLite (sqlx) pool, migrations, repositories
│   ├── sonora-scan/          recursive walker, tag extraction, incremental ingest
│   ├── sonora-playback/      GStreamer engine, transport and queue over core::Queue
│   ├── sonora-artwork/       embedded extraction, conversion, cover export, store
│   ├── sonora-metadata/      keyless providers, LRCLIB lyrics, Last.fm, cache
│   ├── sonora-ui/            Slint markup + the bridge that feeds it
│   └── sonora-app/           binary: wiring, paths, CLI, logging
└── docs/
```

Dependency direction is strictly one-way:

```
sonora-app → sonora-ui → sonora-scan → sonora-db → sonora-core
sonora-ui → sonora-playback → sonora-core
sonora-app → sonora-artwork, sonora-metadata → sonora-core
```

No crate may depend "upward": the database never knows about Slint, the UI never
issues SQL, and the scanner never touches widgets. This is what lets playback,
Last.fm, artwork and device sync be added without rewriting the shell.

*(planned)* `sonora-services` (Last.fm scrobbling, Discord presence),
`sonora-device` (sync) and `sonora-soulseek` slot in beside these, each
depending on `sonora-core` and (where they persist data) `sonora-db`.

## Threading model

- The **main thread** owns the Slint event loop. Slint is not `Send`, so all
  widget mutation happens inside `upgrade_in_event_loop`.
- A **multi-threaded Tokio runtime** runs database queries, network work and
  device transfers.
- **Tag parsing is blocking**; the scanner therefore runs the directory walk on
  `spawn_blocking` and streams batches of 128 tracks to the async side over a
  bounded channel. Memory stays flat regardless of library size.
- The scanner publishes `Event`s on a `tokio::sync::broadcast` channel. The UI
  subscribes once and translates them into status text and refreshes. Progress
  is throttled to ~150 ms so a fast scan cannot flood the event loop.
- Search responses carry a generation number; a stale result is discarded, so
  fast typing cannot display an older query's rows.

## Data model (migrations `0001_init.sql`, `0002_lyrics.sql`)

| Table | Purpose |
|---|---|
| `library_folders` | Registered roots, last scan time |
| `artists` | Unique display name + precomputed `sort_name` (articles stripped) |
| `albums` | `(title, album_artist_id)` unique, optional year |
| `tracks` | One row per file; `path` unique; technical metadata inline |
| `genres`, `track_genres` | Many-to-many; a file tagged "Rock; Pop" gets both |
| `tracks_fts` | FTS5 index over title/artist/album/album artist |
| `lyrics` | Fetched lyrics per track (plain + LRC), cascading on delete |
| `settings` | Typed key/value store (JSON values) |
| `scan_runs` | Audit trail: what each scan did |

Notable choices:

- **`last_seen_scan`** on `tracks` marks rows visited by the current run. Rows
  this run did not touch are deleted afterwards, so deletions propagate without
  a full-library diff and without deleting data if a scan is interrupted.
- **`tracks_fts` is a standalone FTS5 table**, not contentless: a row can be
  deleted by `rowid`, which keeps incremental updates simple.
- Users' queries are tokenised, quoted and prefix-matched (`"aphex"*`), so
  punctuation cannot inject FTS syntax.
- **User settings are one JSON document** under the `settings` key instead of a
  column per option. `Settings::normalized` clamps every range (bitrate ceiling,
  cache budget, artwork quality) and restores a blank folder template on both
  load and save, so a hand-edited database or a document written by an older
  version can never produce an unusable state. An unreadable document is logged
  and replaced by defaults rather than blocking startup.
- Album artists fall back to the track artist. Files with no album tag are not
  forced into a fake album.

## Incremental scanning

1. The requested folder is canonicalised and registered.
2. A `scan_runs` row is opened.
3. Supported extensions are walked; hidden directories, `$RECYCLE.BIN`,
   `System Volume Information`, `__MACOSX` and `node_modules` are skipped.
4. Each file is read with `lofty`. Missing titles/artists are recovered from the
   file name (`Artist - 01 - Title`), never invented when the name has no
   separator.
5. Batches are upserted in one transaction. A file whose size and mtime are
   unchanged is left alone and only marked as seen.
6. Untouched rows are pruned; albums and artists left empty are removed.
7. The run row and the folder's `last_scan_at` are finalised and stats are
   published.

## UI structure

A single window: fixed sidebar, header with global search, and a content region
that switches on `section` (`home`, `tracks`, `albums`, `folders`). The palette
is a Slint global (`Palette`) so the planned light theme and accent picker only
have to override that one place.

Model state (track lists, album list, folder list, counters) is pushed from Rust
as fresh `ModelRc`s on every refresh. There is no two-way binding of library
data: SQLite remains the single source of truth.

## Deliberate open decisions

- **Gapless playback drives `playbin`**, not `playbin3`: `playbin3` has no
  `about-to-finish` signal, and that signal is what makes the handover exact.
  `GstPlayer` is rejected for the same reason — it does not expose the queue
  semantics Sonora needs.
- **ReplayGain** will be read from tags and applied by the engine, with
  `rgvolume` only as an analysis fallback.
- **Artwork** will be content-addressed (blake3 → cached JPEG renditions) so the
  batch PNG/WebP→JPEG job is idempotent and resumable.
- **Device sync** identifies volumes by serial + a marker file, never by drive
  letter, because the Y1 is plain USB mass storage that can be remounted under a
  different path.
- **Android packaging** will decide between `cargo-apk` and a Gradle harness with
  Rust built via `cargo-ndk`. The GStreamer Android runtime decides whether
  playback is shared code or a platform-specific backend, so that spike comes
  before the touch layout is finalised.
- **Discord presence will sit behind a trait** with two backends: the `discord-
  rich-presence` RPC client on desktop, and the Discord Social SDK's C++ library
  through FFI on Android (the Social SDK ships for C++, Unity and Unreal, not
  Rust). The presence payload — track, artist, elapsed/remaining time, paused
  state — is identical on both, so the trait boundary is the payload builder plus
  a `set_activity` call, and the Android backend is the only extra packaging
  cost.
