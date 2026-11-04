# Sonora

A native cross-platform music library for Windows, Linux and Android — Rust
core, Slint interface, GStreamer playback, SQLite storage. No Electron, no
bundled browser.

## Status

Sonora is under active development. What works today:

- Recursive library scanning of MP3, FLAC, M4A/AAC, OGG, Opus, WAV, AIFF,
  WavPack, Monkey's Audio, Musepack and WMA files.
- Tag extraction via `lofty`, with file-name fallback only when tags are absent.
- Incremental rescans: unchanged files are skipped, deleted files are pruned.
- SQLite library with FTS5 search, album/artist aggregation and library stats.
- Slint shell: Home, Tracks, Albums and Library folders, plus a transport bar,
  per-row play/queue actions and a drag-reorderable queue panel.
- GStreamer playback over `playbin`: play/pause, previous/next, seek, volume and
  gapless handover between queued tracks.
- Artwork pipeline: embedded-cover extraction, JPEG/PNG conversion with a size
  cap, `cover.jpg` export and a content-addressed cache.
- Keyless metadata and lyrics: MusicBrainz and Cover Art Archive, Deezer and
  iTunes artwork fallbacks, LRCLIB lyrics (synced and plain), all cached;
  Last.fm tags when a key is configured.
- Settings screen for the format preference and bitrate ceiling, cache budget,
  artwork format and quality, folder template and the Last.fm key.
- First-run onboarding (folders, format, optional Last.fm) that can be replayed
  from settings.

Supported platforms: **Windows**, **Linux** and **Android**. Android is a
first-class target, but its build harness and touch layout are still being
built, so today only the desktop targets produce a runnable binary; the plan and
acceptance criteria are in the [Android track](docs/ROADMAP.md#android-track-parallel-from-phase-2).

Last.fm scrobbling, Discord presence, the artwork review screen, device sync,
downloads and the Android build harness are planned and documented in
[docs/ROADMAP.md](docs/ROADMAP.md).

## Building

All commands below build the host target (Windows or Linux). Android is built
through the NDK harness described in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md),
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

## Documentation

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — crate layout, threading, schema
- [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) — setup, conventions, testing
- [docs/ROADMAP.md](docs/ROADMAP.md) — phases and acceptance criteria
- [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) — product requirements for the
  later phases (downloads, metadata, queue rules, cache, sync, onboarding)
- [docs/LICENSING.md](docs/LICENSING.md) — dependency licence audit

## Licence

Sonora is free software, licensed under the GNU General Public License v3.0 or
later. This is required by the GPLv3 option of the Slint UI toolkit.
