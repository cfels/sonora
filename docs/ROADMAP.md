# Roadmap

Each phase ends with something a person can actually use. A phase is only done
when its acceptance criteria pass.

Product requirements collected for later phases (Soulseek identity modes,
sharing, keyless metadata and artwork, queue rules, format preference, cache
eviction, device sync, folder layout, onboarding) live in
[REQUIREMENTS.md](REQUIREMENTS.md). That document is the source of truth for
what each phase has to cover.

## Phase 1 — Foundation ✅

Delivered:

- Cargo workspace with one-way crate dependencies.
- SQLite schema, migrations, repositories, FTS5 search.
- Recursive incremental scanner with tag extraction and file-name fallback.
- Slint shell: sidebar navigation, Home, Tracks, Albums, Library folders,
  global search, live scan status.
- CLI: `--scan`, `--where`, `--version`.
- Unit and integration tests for parsing, database behaviour and scanning.

Acceptance: scanning a folder twice imports then reports everything unchanged;
deleting a file removes it from the library on the next scan. ✅

## Phase 2 — Playback and library (in progress)

Delivered:

- `sonora-playback`: GStreamer engine over `playbin` (not `playbin3`, which has
  no `about-to-finish`), gapless handover, play/pause/stop, previous/next, seek,
  volume, the full queue API and a caller-supplied track resolver.
- Apple-Music-style queue rules (shuffle only upcoming, play next, add to queue,
  drag reorder, repeat off/all/one), tested in `sonora-core::queue`.
- Transport bar (track, play/pause, previous/next, click-to-seek, volume),
  per-row play/Next/Add actions and a drag-reorderable queue panel.
- Queue persistence: the queue snapshot survives a restart, including shuffle
  state and repeat mode.
- Settings: format preference and ceiling, cache budget, artwork format/quality,
  folder template, the Last.fm key, native download/cache folder pickers and the
  device-sync opt-in.
- Play activity: history, play and skip counts, star ratings and favourites,
  written by the UI, shown on Home (Most played), History and Favourites, and
  readable headlessly with `sonora --history`.
- ReplayGain from tags: the scanner reads track and album gain and peak
  values, the database persists them, and the engine applies the track gain
  (album gain as the fallback) as a linear volume factor so loudness is even
  across files. Tracks carrying a gain show it as a badge in the list.
- Output device selection: the Settings screen opens a picker listing the
  GStreamer sinks this system actually has; the choice is stored in settings
  and switched on the live pipeline immediately, not just at the next start.
- Album pages: clicking a release in Albums opens its tracks in disc/track
  order (untagged files last), with one action that plays the whole album
  and the usual per-row play, queue, rating and favourite controls.
- Artist pages: the artist credit on an album opens everything by that
  artist — their albums and every track they perform — with one action that
  plays them all. Back returns to the release the artist was opened from.

Still open:

- Artwork in the rows and on album cards instead of placeholders.
- Custom playlists and smart playlists with a rule editor.
- MPRIS (Linux) and SMTC (Windows) media controls; Android uses its platform
  media session instead (see the Android track).

Acceptance: playback survives restart with its queue; play counts and history
are written per track; a smart playlist updates live as rules change.

## Phase 3 — Artwork (in progress)

Delivered:

- Embedded artwork extraction, JPEG/PNG conversion with configurable quality and
  max dimension, alpha flattened onto white, `cover.jpg`/`cover.png` export that
  never replaces an existing cover without approval, blake3 content-addressed
  store, batch report.

Still open:

- Candidate review UI, embedded artwork writing, Rockbox layout.

Acceptance: a 2 000-album library converts without corrupting any tag, and the
review screen shows every proposed change before it is applied.

## Phase 4 — Online (in progress)

Delivered:

- `sonora-metadata`: MusicBrainz → Cover Art Archive, Deezer and iTunes artwork
  fallbacks, LRCLIB synced and plain lyrics, Last.fm tags behind an API key, TTL
  disk cache, every provider behind an `Http` trait so tests never touch the
  network.
- Lyrics stored per track in the `lyrics` table, and reachable headlessly with
  `sonora --lyrics <query>` and `sonora --cover <query>`.
- A lyrics pane in the player: it shows saved lyrics first (so a second open is
  offline and instant), fetches from LRCLIB on demand, keeps the result, and
  clears itself when the track changes instead of showing the previous song.
- Cover art in the transport bar, decoded off the UI thread from the file's
  embedded picture and refreshed whenever playback moves to another track.

Still open:

- Artist and album profiles with pictures.
- Last.fm (needs a free developer API key): auth, scrobbling with an offline
  queue, now-playing, loved tracks, charts, similar artists, recommendations,
  cached responses.
- Synced lyrics that follow playback line by line, local `.lrc` precedence over
  remote results, and lyric editing.
- Discord Rich Presence (hidden while paused, fully disableable): RPC/IPC on
  desktop, and on Android through the Discord Social SDK, which publishes
  presence without account linking when the Discord app is installed and signed
  in (SDK 1.10+, Android 7.0+).
- Discovery homepage that degrades to local data when offline.

Acceptance: an offline session's scrobbles replay correctly on reconnect; the
homepage renders with the network unplugged.

## Phase 5 — Devices (Windows and Linux)

- Volume detection: Linux (mount table + udev) and Windows (SetupAPI). Android
  has no device sync at all; see the Android track.
- Sync profiles, diff planning against a device manifest, resumable copies with
  progress, free-space checks, safe unplug handling.
- Rockbox-compatible artwork export. The Y1 is treated as plain USB mass
  storage; Rockbox is an option, never an assumption.

Acceptance: unplugging mid-transfer and re-plugging resumes without duplicates.

## Phase 6 — Downloads

- Spike: `soulseek-rs-lib` versus an external `slskd` daemon, judged on licence,
  stability and packaging cost. Backend hidden behind a trait either way.
- Identity modes: sign in with a real account, or opt into a rotated random
  identity with its rate-limit and ToS caveats surfaced in the UI.
- Sharing gate: shared-folder uploads, or a clear explanation when a peer
  refuses downloads from non-sharing users.
- Search with filters, transfer queue, history, import with metadata and art
  into the user's chosen download folder, laid out by the folder template.
- Legal acknowledgement before first use.

## Android track (parallel from phase 2)

Android is a first-class target, so its work runs alongside the desktop phases
instead of after them.

- NDK build harness (`cargo-apk` or a Gradle + `cargo-ndk` project) producing an
  APK that links the Slint Android backend and a GStreamer Android build of the
  same plugin set, for arm64-v8a plus an emulator ABI.
- Responsive, touch-first layout: bottom navigation, single-column lists, a
  queue sheet, larger hit targets, no hover-only actions.
- Library access through the Storage Access Framework, with the same incremental
  scan and FTS search as desktop.
- Playback in a foreground service with a media notification and lock-screen
  controls, surviving backgrounding and screen-off.
- Downloads and cache inside app-scoped storage, with an optional user-granted
  folder; no external device sync (a phone is a playback client, never a sync
  host).
- Discord Rich Presence through the Discord Social SDK, published without account
  linking when the Discord app is installed; system media controls through
  `MediaSession` and the media notification.

Acceptance: on a physical Android device, pick a folder in the system picker,
scan it, play a track gaplessly with the screen off, and get the same queue back
after the app is killed and reopened.

## Phase 7 — Polish (in progress)

Delivered:

- First-run onboarding: welcome, music folders, format preference and the
  optional Last.fm key, skippable at every step, plus **Run onboarding again** in
  settings. The Soulseek sharing notice and device setup join the flow when
  those features exist, rather than being shown before they mean anything.
- Packaging: Windows installer, Linux AppImage/Flatpak, signed Android APK/AAB.
- Keyboard shortcuts, accessibility pass, error reporting, performance work.
- Format coverage validation against a real test corpus.
