# Development

## Requirements

- Rust 1.80 or newer (`rustup` recommended; `rust-toolchain.toml` pins stable).
- A C toolchain (`cc`) and `pkg-config`.
- **Linux:** `libfontconfig1-dev` is required to build the Slint backend.
  From phase 2 onwards the GStreamer development packages are required too:

  ```sh
  sudo apt install build-essential pkg-config libfontconfig1-dev \
      libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
      gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
      gstreamer1.0-plugins-bad gstreamer1.0-libav gstreamer1.0-pulseaudio
  ```

- **Windows:** MSVC build tools and the GStreamer MSVC runtime (phase 2+).
- **Android (arm64-v8a, x86_64 for the emulator):** the
  `aarch64-linux-android` and `x86_64-linux-android` Rust targets, the Android
  NDK (r26 or newer), JDK 17, `cargo-apk` (or `cargo-ndk` for the Gradle
  harness), and a GStreamer Android build matching the desktop plugin set.
  The APK harness and its CI job are **not in this repository yet**; they land
  with the first Android build target, so treat this list as the intended setup
  rather than a validated one.

## Build and test

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

These build the host (Windows or Linux) target. Android builds go through the
NDK harness described above; the crate code, tests and lint rules are the same
for every target.

Run the application:

```sh
cargo run -p sonora-app
```

Index a folder without a display (also the fastest way to smoke-test the
pipeline):

```sh
cargo run -p sonora-app -- --scan ~/Music
SONORA_DATA_DIR=/tmp/sonora cargo run -p sonora-app -- --where
```

Inspect the persisted settings document (same JSON the settings screen writes):

```sh
cargo run -p sonora-app -- --settings
```

Look up metadata for a track already in the library (these two need a network
connection; the first run stores the result):

```sh
cargo run -p sonora-app -- --lyrics "Radiohead Creep"
cargo run -p sonora-app -- --cover "Radiohead Creep"
```

Tests use in-memory SQLite, generated WAV fixtures and canned provider
responses, so they need no network, no audio device and no display. Playback
tests drive a real GStreamer pipeline into `fakesink`.

## Conventions

- **Domain types live in `sonora-core`.** If two crates need to agree on a
  shape, it belongs there.
- **SQL only lives in `sonora-db`.** No other crate writes a query.
- **Blocking work never runs on the UI thread or on the async reactor.** Use
  `spawn_blocking` and stream results.
- **No fake UI.** A control is either wired to real behaviour or it is absent.
- **Every user-visible failure is an `Event` or a status message**, not a
  `unwrap`.
- **New settings go in `sonora-core::settings`.** Add fields with
  `#[serde(default)]` so documents written by older versions still load, and
  clamp ranges in `Settings::normalized` instead of trusting callers.
- **File paths in comments/docs use the repository layout**, not anyone's
  machine.

## Adding a feature

1. Model it in `sonora-core` (types + events).
2. Persist it in `sonora-db` with a new migration and repository methods.
3. Implement the work in a service crate; publish progress as `Event`s.
4. Wire it into `sonora-ui` last.
5. Test the business logic, not the pixels; update `docs/ROADMAP.md`.

## Environment variables

| Variable | Effect |
|---|---|
| `SONORA_DATA_DIR` | Override the library data directory (tests, portable installs) |
| `RUST_LOG` | Log filter, e.g. `RUST_LOG=debug` |

## Sandbox note

The development container used for this phase has no root access and no
fontconfig development package, so the UI binary was linked against a local stub
purely to compile and run the headless CLI there. On any normal desktop install
the `libfontconfig1-dev` package above is what you need; nothing about the stub
is part of this repository.
