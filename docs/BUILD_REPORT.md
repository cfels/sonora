# Build and test report

## Environment and baseline

- Linux x86_64, NixOS 26.11; rustc 1.99.1 (2026-10-13).
- Fontconfig 2.18.3; GStreamer 1.28.7; locked Slint 1.18.1.
- Original baseline: `5fb95d0f5b4199eb950cd7c3c86b8ba172153b92`.
- Plain Cargo build reproduced the supplied `yeslogic-fontconfig-sys` failure:
  `pkg-config` could not locate `fontconfig.pc`.
- Building the preserved original inside the provisioned Nix shell succeeded;
  its original workspace suite passed 156 tests.

`shell.nix`, `flake.nix` and `nix/dev-shell.nix` now provide actual development
libraries and runtime plugins. There is no fontconfig stub, global package
installation or system configuration change. `flake.lock` pins nixpkgs at
`f9a75f41be3a74eab55d3a7679d34ad6cc0da977`.

## Commands and final results

Run from the source root:

```sh
nix-shell --run 'pkg-config --modversion fontconfig gstreamer-1.0'
nix-shell --run 'cargo fmt --all -- --check'
nix-shell --run 'cargo check --workspace'
nix-shell --run 'cargo clippy --workspace --all-targets -- -D warnings'
nix-shell --run 'cargo test --workspace'
nix-shell --run 'cargo build --workspace'
nix-shell --run 'cargo run -p sonora-app'
```

The dependency probe was run directly through this repository's `nix-shell`.
Build/check/test commands were run through `nix develop path:/tmp/sonora-nix
--command ...`, a minimal provisioned shell with the same packages. This avoids
copying the checkout's multi-gigabyte `target/` to the Nix store. The final source
ZIP can also use its flake normally. See DEVELOPMENT.md.

| Check | Result |
| --- | --- |
| Rust format check | Passed |
| Official Slint 1.18.1 formatter | Applied to `app.slint` |
| Workspace check | Passed |
| Clippy, all targets, warnings denied | Passed |
| Workspace tests | 180 passed, zero failures |
| Workspace debug build | Passed; runnable `sonora` binary produced |
| Git whitespace check, CRLF-aware | Passed |

Test distribution: artwork 11, cache 19, core 51, database 32, metadata 24,
playback 17, scanner 6, UI 20. Binary/doc-test targets add no tests. The UI suite
includes pointer events and real software-rendered pixels; the playback suite
uses real GStreamer and generated WAV fixtures with `fakesink`.

The manifests now advertise Rust 1.96 because locked `kstring 2.0.5` requires
1.96; locked Slint and GStreamer bindings already require 1.92. Compilation was
verified on 1.99.1, not separately on the exact minimum compiler.

## Runtime checks

The actual debug executable ran under Xvfb at 1920×1080 and 920×600 with
`SLINT_BACKEND=winit-software`, `SONORA_DATA_DIR=/tmp/sonora-smoke/data` and
`playback_device=fakesink`. The host's real library and settings were untouched.

Generated fixtures included five 600-second FLAC files with embedded design
covers/ReplayGain and 150-line LRCs, plus 600 long-name WAV tracks. The 600-file
incremental scan imported all tracks with zero errors (79 ms in the recorded
run); this is one synthetic sample, not a general performance benchmark.

Exercised/captured:

- Start, play/pause, next/previous and paused seeking; clock/progress updates.
- Rapid row-title clicks, cover hit targets, selected row and real search results.
- Queue/lyrics mutual exclusion, automatic track refresh and manual lyric scroll.
- Volume/mute persistence and restoration; appearance switches and backgrounds.
- Full-width Settings rails, PNG/JPEG visibility and quality preservation.
- History choice persistence and Home/Playlists/Settings navigation.
- Cache confirmation and deletion: intended cache file removed; queued/unowned
  files, original music and database metadata retained.
- Long rows, fallback covers and narrow layout.

Automated coverage supplies actual retention deletion/boundary assertions,
favourites/settings preservation, ReplayGain mixer values, invalid/live-sink
operations, gapless boundaries, repeat sessions, failed-scan protection and
stale-state handling. GUI observations alone are not counted as assertions of
all those behaviors.

## Platform and coverage limits

- Windows/macOS runtime testing was unavailable. The output-switch fix avoids
  live replacement on every platform, but Windows crash resolution is not
  claimed as runtime-verified.
- Physical audio output/latency, hardware disconnect/reconnect and individual
  device selection were not tested. `fakesink` verifies pipeline behavior and
  mixer properties without sound output.
- Live LRCLIB/metadata services, desktop file-picker portals, Android packaging,
  long-session memory/CPU behavior and every keyboard/screen-reader workflow
  were not fully exercised.
- Xvfb emitted display-size/XSETTINGS warnings; its session lacks an activatable
  desktop portal. These are recorded environmental warnings. No successful
  Windows, release-profile or APK build is implied.
- No whole-app pixel-perfect claim: FIGMA_REPORT.md explains missing frames,
  reconstructed references, font differences and blur approximation.

Logs and selected screenshots accompany the delivery. Packaging additionally
checks ZIP integrity and applies the standalone patch to a clean baseline.
