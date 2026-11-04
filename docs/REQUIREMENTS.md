# Product requirements

Direction captured for the phases after playback. These are commitments to build,
not idle wishes: each section records what was asked for, the constraints that
turn out to matter when implementing it, and any risk worth deciding on before
code is written.

## Soulseek downloads (phase 6)

Soulseek is peer-to-peer: Sonora's backend is a peer with its own
username/password, like a torrent client, not a client of an HTTP API. That
shape decides the whole design.

**Two identity modes**

1. **Account mode** — the user signs in with an existing Soulseek account. Their
   sharing, upload slots and buddy list behave exactly like any other client.
   This is the mode a returning user wants.
2. **Private mode** — a randomly generated username and password, rotated every
   download or every few downloads, so downloads cannot be strung together into
   one identity.

Private mode has real costs that must be visible in the UI, not hidden:

- The Soulseek server rate-limits logins per IP, so aggressive rotation will be
  refused. Rotation is therefore opt-in with a sensible interval (a small
  handful of downloads by default), and the app backs off and reports it.
- Rotating mid-transfer abandons the transfers still queued under the previous
  identity; those must be re-queued or explicitly cancelled.
- Uploads and shares travel with the identity, so private mode should not
  advertise sharing until a profile is created for it.
- Terms of service check required before shipping this: ban evasion or
  anonymity is not an approved use by the network's operators. Record the
  outcome in `docs/LICENSING.md` next to the backend decision.

**Sharing is a gate, not a courtesy**

Many Soulseek users refuse downloads from peers sharing nothing. So Sonora
either:

- offers a shared-folder upload feature (folder list, share names, upload
  limits), or
- tells the user plainly, at onboarding and when a peer rejects a transfer,
  that they need to share through another Soulseek client first.

Never present a rejection as a network error: "peer only accepts downloads from
users who share files" is the message, plus the shortcut to the sharing
setting.

**Backend** stays behind a trait (`soulseek-rs-lib` versus an external `slskd`
daemon), so the identity modes above are policy on top of either. `slskd` gives
us its own account model; a library gives us direct control. Decide in the spike.

## Metadata, profiles and artwork (phase 3–4)

Wanted: artist profiles and pictures, album covers and per-track artwork.

Reality of the sources:

| Source | Key needed | Gives |
|---|---|---|
| MusicBrainz | none, but a descriptive `User-Agent` and ≤1 req/s | artist/album/track IDs, relationships, biographies via linked Wikidata |
| Cover Art Archive | none | front/back covers, per release |
| Deezer API | none | artist pictures, album covers, 30 s previews |
| iTunes Search API | none | artwork at several sizes, artist pages |
| Last.fm | **always an API key** | biographies, similar artists, tags, charts, scrobbling |
| LRCLIB | none | plain and line-synced (LRC) lyrics |

So the plan is: keyless sources first (MusicBrainz + Cover Art Archive, Deezer,
iTunes) for profiles, pictures and covers; Last.fm as the enrichment layer that
needs a free developer API key, which also unlocks login, scrobbling and
now-playing. Every remote response is cached on disk with an expiry, and the UI
degrades to artwork-free rows when offline rather than showing broken images.

Lyrics: LRCLIB first (it also returns synced lyrics), local `.lrc`/embedded
lyrics win over remote, all responses cached. Rate limits are respected and a
missing lyric is a normal state, not an error.

Artwork downloads are the user's own responsibility for copyright; Sonora stores
them locally and does not redistribute.

## Queue behaviour (phase 2)

Apple Music semantics, implemented in `sonora-core::queue` with tests:

- Shuffle affects **only upcoming tracks**. The playing track and history are
  never reordered or replayed by shuffling.
- **Play next** inserts at the top of the queue (immediately after the current
  track); **add to queue** appends to the end.
- The queue list is drag-reorderable; a manual order becomes the intended order,
  so turning shuffle off restores what the user sees.
- Repeat off / all / one.

## Playback format preference

A setting with two tiers:

- **Lossless preferred** — FLAC, WAV (and ALAC) win wherever a choice exists.
- **Lossy preferred** — MP3, OGG/Vorbis, Opus, AAC, with a **maximum bitrate**
  slider that acts as a ceiling, never a target to upsample to.

It governs downloads, device-sync transcoding and what the cache is allowed to
replace. It never changes how existing library files are played.

## Track cache

- Tracks fetched for playback (streamed or downloaded sources) are cached on
  disk in a dedicated directory.
- The size limit is adjustable in settings.
- Eviction is least-recently-played first, and also runs when free disk space
  drops below a safety floor.
- The currently playing track, and anything queued behind it, is never evicted.
- Eviction is logged so the user can see what was dropped.

## Device sync (phase 5, Windows and Linux only)

**Desktop-only by design.** Android does not ship this feature at all: a phone is
a playback client, never a sync host. No volume-detection service, no sync
section in settings, and no device-sync code in the Android build — the feature
is compiled out rather than shown and disabled.

- Automatic sync is **opt-in**. When enabled, detecting the device starts a sync
  of the device's chosen folder against its paired folder on the PC.
- Each device stores which PC folder maps to which device folder; manual "sync
  now" is always available.
- Removable media is identified by volume identity, never drive letter.

## Library folder layout

MusicBee-style segregation, configurable template, defaults to:

```
<root>/<Album Artist>/<Album>/<NN> <Title>.<ext>
<root>/<Album Artist>/<Album>/cover.jpg
```

Artwork written into files or beside them as **JPEG or PNG, user-selectable**,
with the existing quality/size settings from the artwork phase. Compilation
handling, "unknown artist" buckets and multi-disc numbering are part of the
template, not special cases.

## Onboarding

First run walks through, in order: data folder, music folders to index, format
preference, Last.fm connection (skippable), the Soulseek sharing notice, and
optional device setup. It must be completable offline and skippable at any step,
leaving a working library behind.

Settings has a **Run onboarding again** button that replays the same flow
without touching existing library data.

## Android

Same library, same queue, same downloads — different storage and lifecycle rules.

- Folder access via the Storage Access Framework. No raw paths and no "scan the
  whole SD card" permission.
- Playback in a foreground service with a media notification and lock-screen
  controls, so the queue survives backgrounding and screen-off.
- Downloads and cache write into app-scoped storage by default, with an optional
  user-granted folder.
- No external device sync: Android is a playback client, and the Y1 or any other
  USB mass storage device is synced from the desktop app only.
- System media controls come from `MediaSession` and the media notification, not
  MPRIS, which is a Linux D-Bus interface.
- Discord Rich Presence works on Android through the Discord Social SDK. Presence
  can be published without account linking, as on desktop, when the Discord app
  is installed and signed in; RPC presence needs Social SDK 1.10+ and Android
  7.0+, plus an application with the Social SDK enabled. This is documented by
  Discord at <https://docs.discord.com/developers/discord-social-sdk/core-concepts/mobile>
  and the SDK ships as C++/Unity/Unreal, so a Rust binding is part of the work.
- Onboarding has to complete from a cold install with no permissions granted yet,
  and must not assume a keyboard.

## Open questions

- Soulseek library licence and the rotating-identity ToS question (phase 6 spike).
- Whether device-sync transcoding is in scope for the Y1, or copy-only with the
  format ceiling applied at download time. Encoders add a packaging cost
  (AAC/MP3 in GStreamer means the `ugly`/`bad` plugin split).
- Last.fm API key registration needs a public application name and callback;
  decide who owns that key for open-source builds.
