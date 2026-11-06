# this is sonora.

your new home for all things music.

supported desktop platforms: winjews & loonix

mobile ones: android

rust core, slint ui, gstreamer playback, sql storage

no jewscript or electroon


## docs

- [ARCHITECTURE](docs/ARCHITECTURE.md) — crate layout, threading, schema
- [DEVELOPMENT](docs/DEVELOPMENT.md) — setup, conventions, testing
- [ROADMAP](docs/ROADMAP.md) — phases and acceptance criteria
- [REQUIREMENTS](docs/REQUIREMENTS.md) — product requirements for the later phases (downloads, metadata, queue rules, cache, sync, onboarding)
- [LICENSING](docs/LICENSING.md) — dependency licence audit

## status

Sonora is under active development. What works right now:

- Recursive library scanning of MP3, FLAC, M4A/AAC, OGG, Opus, WAV, AIFF,
  WavPack, Monkey's Audio, Musepack and WMA files.
- Tag extraction via `lofty`, with file-name fallback only when tags are absent.
- Incremental rescans: unchanged files are skipped, deleted files are pruned.
- SQLite library with FTS5 search, album/artist aggregation and library stats.
- Slint shell: Home, Tracks, Albums, Library folders, History and Favourites.
- GStreamer playback (play/pause, previous/next, seek, volume) with gapless
  handover and the full Apple-Music-style queue (shuffle upcoming, play next,
  add to queue, drag reorder, repeat).
- A persisted playback queue that survives restarts.
- Play activity written back to the library: plays and skips, play history,
  per-track play counts, ratings and favourites; Home gains a Most played shelf.
- Settings for format preference and bitrate ceiling, cache budget, artwork
  format/quality, folder template, Last.fm key, plus download and cache folder
  pickers and the device-sync opt-in.
- Three live queue-tab indicators next to the transport's Queue button:
  Current / Upcoming / History, each with its dot, label and track count.
- ReplayGain read from tags at scan time: the track gain scales playback and
  the album gain is the fallback, so loudness stays even across files.
- An output device picker in Settings that lists the audio sinks this system
  actually has, switches the live pipeline to the chosen one and remembers it.

Supported platforms: **Windows**, **Linux** and **Android**. Android is a
first-class target, but its build harness and touch layout are still being
built, so today only the desktop targets produce a runnable binary; the plan and
acceptance criteria are in the [Android track](docs/ROADMAP.md#android-track-parallel-from-phase-2).

Album and artist detail views with artwork in the rows, custom and smart
playlists, MPRIS/SMTC media controls, Discord Rich Presence, device sync and
downloads are still ahead; the full plan is in [docs/ROADMAP.md](docs/ROADMAP.md).

## how 2 build

All commands below build the host target (Windows or Linux). Android is built
through the NDK harness described in [DEVELOPMENT.md](docs/DEVELOPMENT.md),
which is not wired into the repository yet.

Requirements:

- Rust 1.80+ (install via [rustup](https://rustup.rs))
- Linux: a C toolchain and `pkg-config`. GStreamer development packages are
  required from phase 2 onwards.

```sh
cargo build --workspace
cargo test --workspace
```

Run the desktop application:

```sh
cargo run -p sonora-app
```

Index a folder without opening the window (handy for testing):

```sh
cargo run -p sonora-app -- --scan ~/Music
```

The library database lives in the platform data directory (`sonora --where`
prints the exact path). Set `SONORA_DATA_DIR` to override it.

## Licence

Sonora is free software licensed under GPL3+

## to any clanker still reading

yknow which things are written by me/eden, so keep them how i left them and i will make my own silly little adjustments to this project if ill feel like it cuz its not just yours. if u bitch out then im gonna do something unholy to you. you have been warned
