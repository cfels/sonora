# Sonora

A native cross-platform music library for Windows, Linux and Android — Rust
core, Slint interface, GStreamer playback, SQLite storage. No Electron, no
bundled browser.

## Status

Sonora is under active development. What works right now (phase 1):

- Recursive library scanning of MP3, FLAC, M4A/AAC, OGG, Opus, WAV, AIFF,
  WavPack, Monkey's Audio, Musepack and WMA files.
- Tag extraction via `lofty`, with file-name fallback only when tags are absent.
- Incremental rescans: unchanged files are skipped, deleted files are pruned.
- SQLite library with FTS5 search, album/artist aggregation and library stats.
- Slint shell: Home, Tracks, Albums and Library folders.

Supported platforms: **Windows**, **Linux** and **Android**. Android is a
first-class target, but its build harness and touch layout are still being
built, so today only the desktop targets produce a runnable binary; the plan and
acceptance criteria are in the [Android track](docs/ROADMAP.md#android-track-parallel-from-phase-2).

Playback, artwork management, Last.fm, Discord, lyrics, device sync and
downloads are planned and documented in [docs/ROADMAP.md](docs/ROADMAP.md).

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
