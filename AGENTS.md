# AGENTS.md

Sonora is a native music library: Rust workspace, Slint UI, SQLite, `lofty` tag parsing. No web view, JavaScript, or Electron.

## Commands

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

CI (`.github/workflows/ci.yml`) runs format check (Linux only), Clippy with `-D warnings`, tests, then build. Run the same checks locally before finishing.

Focused verification:

```sh
cargo test -p sonora-db
cargo test -p sonora-scan scans_a_folder_and_is_incremental
cargo test -p sonora-core queue
```

Desktop app and CLI:

```sh
cargo run -p sonora-app                  # UI; the binary is named `sonora`
cargo run -p sonora-app -- --scan ~/Music
cargo run -p sonora-app -- --artwork ~/Music
cargo run -p sonora-app -- --where
cargo run -p sonora-app -- --settings
cargo run -p sonora-app -- --history
```

`sonora --scan` is the fastest headless smoke test. `--artwork` only processes tracks already indexed under that folder and refuses to replace an existing cover.

Environment:

- `SONORA_DATA_DIR` is a **directory**, not a database file; the library is `<dir>/library.db`. Required on Android.
- `RUST_LOG` controls logging, e.g. `RUST_LOG=debug`.
- Tests use in-memory SQLite and generated WAV/image fixtures. They need no network, audio device, or display.
- Linux needs `pkg-config`, `libfontconfig1-dev` and the GStreamer development packages (`libgstreamer1.0-dev`, `libgstreamer-plugins-base1.0-dev`) to build the full workspace.
- Android is a stated target but has no build/APK harness in this repository; only desktop targets currently produce a runnable binary.

## Workspace

```
crates/sonora-core      domain types, errors, events, queue, settings, helpers
crates/sonora-db        SQLite pool, migrations, all SQL/repositories
crates/sonora-scan      recursive walker, lofty extraction, incremental ingest
crates/sonora-artwork   artwork extraction/conversion/export
crates/sonora-cache     LRU track cache with budget, disk floor and protected queue
crates/sonora-metadata  keyless metadata/artwork providers, LRCLIB lyrics, Last.fm
crates/sonora-playback  GStreamer engine and transport over the core queue
crates/sonora-ui        Slint markup plus the Rust bridge
crates/sonora-app       binary `sonora`: wiring, paths, CLI, logging
```

Actual dependency direction (do not add upward dependencies):

- `sonora-app` depends on `sonora-ui`, `sonora-scan`, `sonora-db`, `sonora-artwork`, `sonora-cache`, `sonora-metadata`, `sonora-core`.
- `sonora-ui` depends on `sonora-scan`, `sonora-db`, `sonora-playback`, `sonora-artwork`, `sonora-metadata`, `sonora-core`.
- `sonora-scan` depends on `sonora-db`, `sonora-core`.
- `sonora-playback` depends on `sonora-core`.
- `sonora-db`, `sonora-artwork`, `sonora-cache` and `sonora-metadata` depend only on `sonora-core`.

`docs/ARCHITECTURE.md` has historically lagged behind the crate set; trust `Cargo.toml` and code over stale prose.

## Architecture rules

- Shared domain shapes live in `sonora-core`. If two crates must agree on a type, put it there.
- SQL lives only in `sonora-db`. Other crates call repository methods, never write queries.
- Tag parsing, filesystem walks, and artwork conversion are blocking: keep them off the Slint event loop and Tokio reactor (`spawn_blocking` in the scanner).
- The scanner streams batches of 128 tracks, publishes `Event`s over a broadcast channel, and throttles progress to ~150 ms.
- Search results carry a generation number; stale async results are discarded.
- Slint is not `Send`; mutate widgets only inside `upgrade_in_event_loop`. Library state is pushed as fresh `ModelRc`s on refresh — SQLite is the single source of truth.
- No fake UI: a control must be wired to real behavior or omitted.
- User-visible failures belong in an `Event`/status message, not an `unwrap`.

## Persistence

- Migrations are embedded at compile time by `sqlx::migrate!("./migrations")`. Add a new numbered file under `crates/sonora-db/migrations/`; never edit an already-applied migration.
- Scans mark visited rows with `last_seen_scan`; untouched rows are pruned afterward. Missing albums/artists are cleaned up too.
- FTS queries are built in `fts_query`: every token is quoted and prefix-matched. Never pass raw user input into `MATCH`.
- Settings are one JSON document under the `settings` key. New fields need `#[serde(default)]`; clamp ranges in `Settings::normalized`. Unreadable settings fall back to defaults instead of blocking startup.

## Current scope

- Playback, the transport, queue persistence, settings, lyrics, artwork extraction and keyless metadata are implemented.
- Play history, play/skip counts, ratings and favourites are written by the UI and shown on Home, History and Favourites.
- Device sync, downloads, the artwork review UI and Android packaging are not implemented.
- Do not add placeholder controls or a fake playback engine to make the shell look complete.

## Dependencies and licensing

- The project is GPL-3.0-or-later; Slint is used under its GPLv3 option.
- Before adding a dependency: confirm it is GPLv3-compatible, does not embed a browser engine, and does not require a proprietary runtime.
- Record every new dependency in `docs/LICENSING.md` in the same commit.
- Avoid `gst-plugins-ugly`; none of the supported formats require it.

## Conventions

- MSRV is Rust 1.80; `rust-toolchain.toml` pins stable with `rustfmt` and `clippy`.
- UI markup lives in `crates/sonora-ui/ui/app.slint`; `build.rs` compiles it and `slint::include_modules!()` exposes generated types.
- Keep the maintainer's existing voice and wording in README/docs unless the task explicitly asks for rewrites.
- Update `docs/ROADMAP.md` when changing implemented behavior that the roadmap tracks.
