# Licensing

## Project licence

**GPL-3.0-or-later.** Slint's free option for open-source applications is GPLv3;
the royalty-free option requires attribution and is aimed at proprietary apps,
and the commercial option is paid. Choosing GPLv3 keeps Sonora fully free and
compatible with every dependency below.

## Audited dependencies

| Crate / component | Version | Licence | Notes |
|---|---|---|---|
| Slint | 1.18.1 | GPL-3.0 / royalty-free / commercial | Used under GPLv3 — this is what makes the project GPLv3 |
| `gstreamer` bindings *(phase 2)* | 0.25.4 | MIT | Bindings only |
| GStreamer core | 1.2x | LGPL-2.1+ | Dynamic linking |
| `gst-plugins-base`, `-good`, `-bad` | — | LGPL-2.1+ (a few files other licences) | Covers FLAC, Vorbis, Opus, WAV, MP3 via mpg123, AAC via faad |
| `gst-plugins-ugly` | — | GPL for some elements | **Avoided**; not required by any supported format |
| `gst-libav` | — | LGPL-2.1+ | Optional extra codec coverage |
| `lofty` | 0.25.5 | MIT | Tag reading/writing |
| `sqlx` (SQLite) | 0.8.6 | MIT / Apache-2.0 | Bundled SQLite is public domain |
| `slint-build` | 1.18.1 | GPL-3.0 / royalty-free / commercial | Build-time only |
| `tokio` | 1.x | MIT | Runtime |
| `walkdir` | 2.x | MIT / Unlicense | Directory walking |
| `chrono` | 0.4 | MIT / Apache-2.0 | Timestamps |
| `thiserror`, `anyhow` | 2 / 1 | MIT / Apache-2.0 | Error plumbing |
| `directories` | 6.x | MIT / Apache-2.0 | Platform data paths |
| `env_logger`, `log` | 0.11 / 0.4 | MIT / Apache-2.0 | Logging |
| `tempfile` (dev) | 3.x | MIT / Apache-2.0 | Test fixtures |

## Planned integrations — licence status

| Item | Status |
|---|---|
| Last.fm API | Free API key; terms require attribution and rate-limit compliance. Credentials are user-supplied, never committed. |
| LRCLIB | Public, no key required; attribution in the UI. |
| `discord-rich-presence` 1.1.0 | MIT (verify at integration time). Desktop RPC/IPC only; requires a registered Discord application id. |
| Discord Social SDK (Android presence) | Proprietary SDK under Discord's Social SDK terms. Free to use, but the redistribution and notice terms for bundling the `.so` in an APK must be read before shipping. Requires an application with the Social SDK enabled. |
| `soulseek-rs-lib` 20.0.0 | **Unverified** — must be checked before the phase 6 spike, and its licence may change between releases. |
| `image` (PNG/WebP→JPEG) | MIT / Apache-2.0. |
| GStreamer Android binaries | Prebuilt shared objects, LGPL-2.1+ with some plugins under other licences. Ship the same subset as desktop (no `ugly`) and include the LGPL notice and source offer in the APK. |

## Rules for new dependencies

1. Confirm the licence is GPLv3-compatible before adding it.
2. No dependency that embeds a browser engine.
3. No dependency that requires a proprietary runtime to build or run.
4. Record every new dependency in this table in the same commit.
