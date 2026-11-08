# Sonora repair report

Baseline: `5fb95d0f5b4199eb950cd7c3c86b8ba172153b92`.

The original checkout was clean apart from the supplied untracked `FIGMA/`.
The local repository and its existing remote were inspected; no prior AI diff,
source archive, or alternate patched checkout was found. The reported previous
fixes were therefore checked against the actual source and implemented here.
The original Git source and Figma file were preserved in
`/tmp/sonora-repair-baseline/` before editing. No user library was opened or
modified during testing. No commit or push was made.

## Reported issues

“Implemented” below describes the code, not a claim of universal platform
verification. Automated tests use real GStreamer with a synchronized `fakesink`;
GUI checks use the actual Slint executable with isolated data and Xvfb.

| Issue | Cause and implemented repair | Verification and limits |
| --- | --- | --- |
| 1. Progress / scrubbing | Replaced the framework slider with an explicitly left-anchored rail. Captured pointer press, motion, release/cancel and keyboard input send real seeks. Duration and seekability gate interaction; clock updates pause during a drag. | Native paused seeking; pointer/render regression proves left fill, continuous callbacks and release beyond bounds; real GStreamer paused seek and invalid seek tests. Hardware/remote streams not exercised. |
| 2. Narrow sliders | Settings rails now stretch within their containers rather than taking the small implicit control width. Labels and values retain their space. | Software-render geometry checks and native full-width Settings capture. The Figma file contains no Settings frame or slider measurements. |
| 3. JPEG controls | JPEG quality is conditionally instantiated only for JPEG; changing to PNG keeps the quality value in state/settings. | Built UI and settings round trips; native settings captures. |
| 4. Lyrics / Queue | Lyrics had an incorrectly constrained content height. The scroll view now measures its real lyric column, highlights the active line, follows playback until manually scrolled, and offers “Follow playback”. Queue and Lyrics close each other. | Pointer scrolling regression, hundreds of lyric lines, native manual scroll and track-change captures. |
| 5. Layout | Rebuilt the player shell from extracted Figma geometry. Constrained long credits and row columns, fixed implicit size/binding issues, and made the main area respond to rail/window sizes. | Native 1920×1080 and 920×600 captures; 600 long-row software rendering. Whole-app pixel equivalence is not established; see FIGMA_REPORT.md. |
| 6. Favourites | Moved entry into Playlists; kept existing repository methods, track records and favourite/rating controls. | Existing database round-trip tests; native Playlists/Favourites navigation. |
| 7. Library folders | Moved folder controls into Settings, including add/browse, rescan and confirmed removal. Removing a root retains track metadata and files. | Scanner/database suite, real headless scans and Settings capture. File-picker interaction depends on the host desktop portal and was not completed under Xvfb. |
| 8. History | Added 13 persisted retention choices, default seven days. Saving settings prunes in the same transaction; startup, history reads and periodic UI cleanup also remove expired rows. Home contains the compact History entry. | Tests prove deletion, cutoff boundary, persisted choice and preservation of lifetime activity/favourites. Month=30 days, six months=180 days, year=365 days; all are rolling UTC durations. |
| 9. Volume | Removed the missing-ReplayGain early-return path. Mixer volume always combines user volume and valid gain; mute is independent. Debounced saves and exit saves preserve volume. | Real mixer property tests without gain, with gain, paused, mute and track changes; native persisted volume/mute and restart. Physical acoustic output was not measured. |
| 10. Automatic lyrics | Opening Lyrics or changing track starts a local-first load. Removed Fetch. Added loading, unavailable and error states, bounded result caching, in-flight deduplication and stale-response guards. | Local LRC/plain/error tests, provider fixtures and native track-change checks. Live network provider availability was not verified. Negative results retry after five minutes; errors after 30 seconds. |
| 11. Output switching | UI saves the output backend for restart. Engine refuses live sink replacement unless stopped with the actual pipeline in NULL. Startup applies the saved backend before playback and handles unavailable backends. | Linux real-pipeline rejection test and startup with saved fakesink. Windows runtime unavailable. Picker lists GStreamer backend factories, not individual physical devices. |
| 12. Clear Cache | Added size/refresh and confirmed clearing of registered track-cache files. Protects queue/current tracks and canonical library originals; skips symlinks/unowned files and reports failures. Fixed deletion of multiple encodings for one track. | Cache tests and native confirmation: owned file deleted, queued/unowned files and originals retained. This button clears the track cache; it does not erase library data, settings or separately managed artwork. |
| 13. Top search | Real debounced FTS query, icon, placeholder, keyboard-editable field, empty-query restoration and generation checks on the UI-thread handoff. Fixed field shrinking after text entry. | Real native search produced four matching covered tracks from a 605-track library. Existing escaped FTS tests; clearing restores the library. |

Status: issues 1–4, 6–10, 12 and 13 are implemented and verified to the extent
listed above. Issue 5 is partially verified: layout is exercised, but whole-app
Figma equivalence is not established. Issue 11 has a stability fix and Linux
coverage; Windows runtime verification is blocked by the unavailable platform.

## Additional fixes and requested additions

- The GStreamer bus previously stopped watching after an idle timeout. It now
  runs until explicit shutdown, joins its thread and reports playback errors.
  A delayed-start regression verifies it remains active after idle.
- Gapless preparation formerly advanced queue/gain before the audible boundary.
  It now stages the next URI and commits on the corresponding stream start,
  preserving queue edits made in the meantime. Real short-file tests cover the
  handover, gain boundary and repeated single-track sessions.
- Repeat-all now repeats a queue containing one track.
- Manual URI changes transition through READY before replacing the URI,
  retaining the same sink. A dedicated serial transport thread keeps native
  transitions off the UI event loop, coalesces superseded pending song clicks,
  and stops/joins on exit. Rapid-switch tests check the final selection and sink.
  This follows the [GStreamer playbin lifecycle](https://gstreamer.freedesktop.org/documentation/playback/playbin.html).
- Playback metadata and selection acknowledge song clicks immediately. Song
  titles and covers activate playback; artist/album credits retain navigation.
  UI-thread stale selection guards prevent older transitions repainting newer
  choices. Song title and cover hit targets have pointer regression coverage.
- Track rows now include asynchronously decoded covers or the Sonora fallback.
  Decode concurrency, pending requests and the in-memory thumbnail cache are
  bounded. Queue thumbnails no longer delay the main cover/background.
- Two Appearance switches independently persist panel blur and blurred
  current-cover backgrounds. Pixel generation runs off the UI thread and uses
  a bounded-resolution fast Gaussian approximation. Cover/background changes
  fade over 140 ms; disabling the effect is also animated. No artwork uses the
  plain background. This is an in-app scene blur, not an OS desktop blur.
- Playback polling records repeated audible sessions separately and refreshes
  Home/History activity after a play.
- Fixed a lock-order inversion risk between queue rendering and the engine's
  resolver by reading the queue before locking track labels.
- Queue drag state now starts at the pressed row rather than defaulting to row
  zero, preventing click-release from reordering a later row.
- Stale search/artwork/lyrics results are rejected. Local lyrics with unreadable
  files report errors; bracketed untimed lyrics remain plain text.
- A failed/incomplete scan no longer prunes library records it could not visit;
  scanner clones serialize overlapping scans. Corrupt-file/incremental tests
  verify the protection.
- Added actual pagination beyond the previous 500-track UI limit.
- Zero-size keyboard focus scopes no longer swallow full-size pointer regions.
- Added pinned Nix development environments with real Fontconfig and GStreamer,
  fixing the reproduced `yeslogic-fontconfig-sys` failure. The dependency set
  actually needs Rust 1.96 (`kstring`), rather than the advertised 1.80. Rust
  1.99.1 was tested; an exact 1.96 toolchain was not separately tested.

## Changed areas

- `sonora-core`: queue behavior, history/appearance settings and normalization.
- `sonora-db`: atomic settings updates, retention cleanup and regression tests.
- `sonora-playback`: pipeline lifecycle, bus, seeking, gain, gapless/session state.
- `sonora-cache`: safe manual deletion and multi-encoding removal.
- `sonora-scan`: serialize scans and avoid pruning after incomplete walks.
- `sonora-ui`: native Slint shell, real controls, asynchronous transport and
  artwork modules, appearance/lyrics/search/settings/navigation and UI tests.
- `sonora-app`: headless settings/history and startup handling.
- Nix files, extracted SVG assets, manifests/lockfile, README and project docs.

No database migration was necessary; new JSON fields have backward-compatible
defaults. No new external Rust dependency was introduced. The UI now uses the
existing internal cache crate.

## Remaining limits

The task is not certified complete for every possible bug or platform. The
supplied design has two player frames, not a complete application specification;
exact typography, blend/blur equivalence and all interaction states cannot be
claimed. Windows, macOS, Android packaging, physical audio hardware and live
network services need their own runtime verification. Tests and screenshots
cover the workflows described here; they do not establish absence of all
races, leaks or long-session failures. See BUILD_REPORT.md and FIGMA_REPORT.md
for reproducible evidence and the exact visual limits.
