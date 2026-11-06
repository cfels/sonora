> [!WARNING]
> this is ai slop
> i am just an unemployed retard whose expertise does not include coding
> 
> i make slight adjustments and will continue until this project fully meets my expectations whilst also maybe being usable for other h*man beings

> [!CAUTION]
> sonora is still an early work in progress. it is **not currently suitable for casual or everyday usage**, and some of the features described below are still being built.
>
> don't assume something exists just because it's mentioned here. check the [status](#status) and [roadmap](docs/ROADMAP.md) before getting too excited.
>
> [track development](https://github.com/takyon303/sonora/commits/main/)

# this is *sonora,* your new home for all things music.

an attempt at bringing your entire music experience together in one place without having to juggle a billion different applications to do so.

**platforms:** winjews & loonix + android (planned)

**built with:** rust · slint · gstreamer · sqlite

**no jewscript or electroon.**

---

## ok but wdym bro

the idea is pretty simple: one application for managing, listening to, discovering and eventually acquiring your music.

rather than being just another music player, sonora is intended to be a complete music library and listening companion.

### the plan

- **your music, your library** — scan, organise, browse and play your local collection, with proper metadata, artwork, history and playlists.
- **online stuff** — discover music, retrieve metadata and artwork, look up lyrics and eventually connect your listening activity to last.fm.
- **soulseek integration** — search for and download music directly through soulseek, with proper transfer management and library integration.
- **device synchronisation** — keep external music players in sync with your desktop library, including compatible artwork and folder layouts.
- **one experience** — a consistent interface across windows, linux and android, with platform-specific features where they make sense.

the point isn't to replace every music service out there. it's to give your own music collection an actual home.

## status

sonora is under active development. the project is being built in phases, and not everything described in the plan is available yet.

### what works right now

**library & organisation**
- Recursive library scanning for MP3, FLAC, M4A/AAC, OGG, Opus, WAV, AIFF, WavPack, Monkey's Audio, Musepack and WMA.
- Metadata extraction through `lofty`, with filename fallback when tags are missing.
- Incremental rescanning: unchanged files are skipped and deleted files are pruned.
- SQLite library with FTS5 search, album/artist aggregation and library statistics.
- Album and artist pages, including album track ordering and artist-wide track browsing.

**playback & queue**
- GStreamer-based playback with play/pause, previous/next, seeking and volume control.
- Gapless playback handover.
- Persistent playback queue that survives application restarts.
- Apple Music-style queue behaviour:
  - Shuffle upcoming tracks without rearranging the currently playing track or history.
  - Play next and add to queue.
  - Drag-and-drop queue reordering.
  - Repeat off / all / one.
- Playback history, play and skip counts, ratings and favourites.
- Most played shelf on Home.
- ReplayGain support, with track gain taking priority over album gain.
- Audio output device selection, including switching devices without restarting playback.

**artwork, metadata & lyrics**
- Embedded artwork extraction and conversion.
- Configurable artwork format, quality and maximum dimensions.
- Content-addressed artwork storage.
- Online metadata providers through MusicBrainz and other sources.
- Cover artwork retrieval with multiple provider fallbacks.
- Lyrics retrieval through LRCLIB, with cached lyrics available offline.
- A lyrics pane in the player and artwork in the playback transport.

**interface & settings**
- Slint desktop interface with Home, Tracks, Albums, Library folders, History and Favourites.
- Global library search.
- Album and artist navigation.
- Format preference and bitrate ceiling settings.
- Cache budget and artwork configuration.
- Configurable library folder template.
- Last.fm API key configuration.
- Download and cache folder pickers.
- Device-sync opt-in setting.
- Live Current / Upcoming / History queue indicators.

### what's still yet to come

the following are either partially implemented or waiting for their respective development phase:

- Artwork displayed throughout album cards and track rows, rather than placeholders.
- Custom playlists and smart playlists with a rule editor.
- Last.fm authentication, scrobbling, recommendations, charts and similar artists.
- Artist and album profiles with pictures.
- Synced lyrics, local `.lrc` handling and lyric editing.
- Discord Rich Presence.
- MPRIS (Linux) and SMTC (Windows) media controls.
- External device detection, sync profiles and resumable transfers.
- Soulseek search, downloads, transfer management and library importing.
- Android build infrastructure, touch-first interface and background playback.
- Final onboarding, packaging, accessibility and performance work.

### platform support

winjews & loonix: desktop build targets
android: first-class planned target; build harness and touch interface still in development

currently the desktop platforms are the only ones producing runnable application binaries

android is being developed alongside the desktop phases, not treated as an afterthought. its implementation has additional requirements around storage permissions, background playback, media sessions and touch interaction. also the library sync feature is intentionally left out

for the actual development phases and acceptance criteria, see the [roadmap](docs/ROADMAP.md).

## docs

- [ARCHITECTURE](docs/ARCHITECTURE.md) — crate layout, threading, database schema and internal structure
- [DEVELOPMENT](docs/DEVELOPMENT.md) — setup, conventions, tooling and testing
- [ROADMAP](docs/ROADMAP.md) — development phases, current progress and acceptance criteria
- [REQUIREMENTS](docs/REQUIREMENTS.md) — detailed product requirements for upcoming features
- [LICENSING](docs/LICENSING.md) — dependency licence audit and related decisions

## how 2 build

for now, these commands build the desktop target

android has its own NDK build requirements, which are documented in [DEVELOPMENT.md](docs/DEVELOPMENT.md), but the harness isn't wired into the repository yet

**requirements**

- rust 1.80+ ([rustup](https://rustup.rs))
- for linux: a C toolchain, `pkg-config` and the required GStreamer development packages.
- for windows: a compatible Rust toolchain and the required GStreamer installation.

**build & test**

```sh
cargo build --workspace
cargo test --workspace
```

**run the desktop application**

```sh
cargo run -p sonora-app
```

**scan a folder without opening the window**

handy for testing and debugging library indexing.

```sh
cargo run -p sonora-app -- --scan ~/Music
```

the library database lives in the platform-specific application data directory.

```sh
sonora --where
```

you can override the location with `SONORA_DATA_DIR`.

for additional setup details, platform-specific requirements and development conventions, consult [DEVELOPMENT.md](docs/DEVELOPMENT.md).

## license

sonora is free software licensed under **GPL-3.0-or-later**.
