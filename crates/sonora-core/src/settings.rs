//! User settings: the single object the settings screen edits.
//!
//! Settings are stored as one JSON document under the `settings` key, because
//! they are read and written as a unit and a half-applied set of options is
//! worse than a stale one. Everything here validates on the way in
//! ([`Settings::normalized`]) so a hand-edited file or an older version cannot
//! produce an out-of-range bitrate ceiling or a cache limit of zero.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{AudioFormat, TrackView};

/// Settings document key in the `settings` table.
pub const SETTINGS_KEY: &str = "settings";
/// Queue document key in the `settings` table.
pub const QUEUE_KEY: &str = "queue";

/// Lowest bitrate ceiling we accept, in kbit/s.
pub const MIN_BITRATE_KBPS: u32 = 32;
/// Highest bitrate a CD-quality lossless stream needs; above this the ceiling
/// stops meaning anything.
pub const MAX_BITRATE_KBPS: u32 = 1411;

/// Default disk budget for downloaded/cached tracks: 4 GiB.
pub const DEFAULT_CACHE_LIMIT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// A cache limit below this would break playback of a single album.
pub const MIN_CACHE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// Which class of file the user wants when Sonora may choose one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatTier {
    /// FLAC/WAV/ALAC/AIFF/WavPack/APE first; lossy files only as a fallback.
    #[default]
    Lossless,
    /// MP3/OGG/Opus/AAC only, never above the bitrate ceiling.
    Lossy,
}

impl FormatTier {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lossless => "Lossless (FLAC, WAV)",
            Self::Lossy => "Lossy (MP3, OGG)",
        }
    }
}

/// Format preference: which tier wins, and the ceiling for lossy files.
///
/// The ceiling is a maximum, never a target: Sonora will not re-encode a
/// 128 kbit/s file up to 320 just to fill it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormatPreference {
    pub tier: FormatTier,
    pub max_bitrate_kbps: u32,
}

impl Default for FormatPreference {
    fn default() -> Self {
        Self {
            tier: FormatTier::Lossless,
            max_bitrate_kbps: 320,
        }
    }
}

impl FormatPreference {
    pub fn new(tier: FormatTier, max_bitrate_kbps: u32) -> Self {
        Self {
            tier,
            max_bitrate_kbps,
        }
        .normalized()
    }

    /// Clamp the ceiling into the range that has a meaning.
    pub fn normalized(mut self) -> Self {
        self.max_bitrate_kbps = self
            .max_bitrate_kbps
            .clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS);
        self
    }

    /// May a file of this format be chosen for download, sync or cache?
    ///
    /// Lossless files are only acceptable in the lossless tier; lossy files are
    /// acceptable in either tier when their bitrate is known and fits under the
    /// ceiling. An unknown lossy bitrate is rejected rather than assumed fine.
    pub fn accepts(&self, format: AudioFormat, bitrate_kbps: Option<u32>) -> bool {
        if format.is_lossless() {
            return self.tier == FormatTier::Lossless;
        }
        bitrate_kbps.is_some_and(|bitrate| bitrate <= self.max_bitrate_kbps)
    }

    /// Preference score for ranking candidates that all pass [`Self::accepts`].
    /// Higher is better; rejected candidates score [`i64::MIN`].
    pub fn score(&self, format: AudioFormat, bitrate_kbps: Option<u32>) -> i64 {
        if !self.accepts(format, bitrate_kbps) {
            return i64::MIN;
        }
        let bitrate = i64::from(bitrate_kbps.unwrap_or(0));
        match self.tier {
            FormatTier::Lossless if format.is_lossless() => 1_000_000 + bitrate,
            _ => bitrate,
        }
    }
}

/// Container artwork is written in when Sonora creates or converts a cover.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtworkFormat {
    #[default]
    Jpeg,
    Png,
}

impl ArtworkFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
        }
    }
}

/// Artwork conversion settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtworkSettings {
    pub format: ArtworkFormat,
    /// JPEG quality, 1–100. Ignored for PNG.
    pub quality: u8,
}

impl Default for ArtworkSettings {
    fn default() -> Self {
        Self {
            format: ArtworkFormat::Jpeg,
            quality: 90,
        }
    }
}

impl ArtworkSettings {
    fn normalized(mut self) -> Self {
        self.quality = self.quality.clamp(1, 100);
        self
    }
}

/// MusicBee-style folder layout, e.g. `{album_artist}/{album}/{track_no:02} {title}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FolderTemplate(String);

/// The template used for new files until the user changes it.
pub const DEFAULT_FOLDER_TEMPLATE: &str = "{album_artist}/{album}/{track_no:02} {title}";

impl Default for FolderTemplate {
    fn default() -> Self {
        Self(DEFAULT_FOLDER_TEMPLATE.to_string())
    }
}

impl FolderTemplate {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self(pattern.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Expand the template into a relative path for `track`.
    ///
    /// Unknown placeholders are left visible rather than silently dropped, and
    /// every path component is sanitised: separators, Windows-reserved
    /// characters and `.`/`..` cannot escape the chosen root.
    pub fn render(&self, track: &TrackView) -> PathBuf {
        let album_artist = track
            .album_artist
            .as_deref()
            .or(track.artist.as_deref())
            .unwrap_or("Unknown Artist");

        let mut components: Vec<String> = Vec::new();
        for segment in self.0.split(['/', '\\']) {
            if segment.trim().is_empty() {
                continue;
            }
            let expanded = expand(segment, track, album_artist);
            let cleaned = sanitize(&expanded);
            if !cleaned.is_empty() {
                components.push(cleaned);
            }
        }
        components.into_iter().collect()
    }
}

fn expand(segment: &str, track: &TrackView, album_artist: &str) -> String {
    let mut expanded = String::with_capacity(segment.len());
    let mut rest = segment;

    while let Some(open) = rest.find('{') {
        expanded.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let token = &after[..close];
                match placeholder(token, track, album_artist) {
                    Some(value) => expanded.push_str(&value),
                    None => {
                        expanded.push('{');
                        expanded.push_str(token);
                        expanded.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                expanded.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    expanded.push_str(rest);
    expanded
}

/// `name` or `name:02`; `None` means the placeholder is unknown and should stay
/// visible in the rendered path.
fn placeholder(token: &str, track: &TrackView, album_artist: &str) -> Option<String> {
    let (name, width) = match token.split_once(':') {
        Some((name, spec)) => (name, spec.parse::<usize>().ok()?.min(12)),
        None => (token, 0),
    };

    let value = match name {
        "album_artist" => album_artist.to_string(),
        "artist" => track.artist_or_unknown().to_string(),
        "album" => track.album_or_unknown().to_string(),
        "title" => track.title.clone(),
        "track_no" => track.track_no.map_or(String::new(), |no| pad(no, width)),
        "disc_no" => track.disc_no.map_or(String::new(), |no| pad(no, width)),
        "year" => track.year.map_or(String::new(), |year| pad(year, width)),
        _ => return None,
    };
    Some(value)
}

fn pad(value: i64, width: usize) -> String {
    if width == 0 {
        value.to_string()
    } else {
        format!("{value:0width$}")
    }
}

/// Make one path component safe on Windows and POSIX at once.
fn sanitize(segment: &str) -> String {
    let mut cleaned = String::with_capacity(segment.len());
    for character in segment.chars() {
        match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => cleaned.push('_'),
            character if character.is_control() => {}
            character => cleaned.push(character),
        }
    }

    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']);
    if trimmed == "." || trimmed == ".." {
        return String::new();
    }
    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Rolling UTC durations. A month is 30 days; six months is 180 days and a
/// year is 365 days. All time disables history deletion, not lifetime counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRetention {
    Hour,
    Day,
    Days2,
    Days3,
    Days4,
    Days5,
    Days6,
    #[default]
    Days7,
    Weeks2,
    Month,
    Months6,
    Year,
    AllTime,
}

impl HistoryRetention {
    pub const ALL: [Self; 13] = [
        Self::Hour,
        Self::Day,
        Self::Days2,
        Self::Days3,
        Self::Days4,
        Self::Days5,
        Self::Days6,
        Self::Days7,
        Self::Weeks2,
        Self::Month,
        Self::Months6,
        Self::Year,
        Self::AllTime,
    ];

    pub fn seconds(self) -> Option<i64> {
        let days = match self {
            Self::Hour => return Some(3600),
            Self::Day => 1,
            Self::Days2 => 2,
            Self::Days3 => 3,
            Self::Days4 => 4,
            Self::Days5 => 5,
            Self::Days6 => 6,
            Self::Days7 => 7,
            Self::Weeks2 => 14,
            Self::Month => 30,
            Self::Months6 => 180,
            Self::Year => 365,
            Self::AllTime => return None,
        };
        Some(days * 86400)
    }

    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|value| *value == self)
            .unwrap_or(7)
    }

    pub fn from_index(index: usize) -> Self {
        Self::ALL.get(index).copied().unwrap_or_default()
    }
}

/// Native player effects. Both can be disabled independently without changing
/// the cover image shown by the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub blur_panels: bool,
    pub cover_background: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            blur_panels: true,
            cover_background: true,
        }
    }
}

/// Everything the user can change, in one document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub format: FormatPreference,
    /// Where downloads are saved. `None` means "inside the library default".
    pub download_folder: Option<PathBuf>,
    /// Where cached tracks live. `None` means "derived from the data directory".
    pub cache_folder: Option<PathBuf>,
    /// Cache budget in bytes; the least recently played tracks are evicted first.
    pub cache_limit_bytes: u64,
    /// Last.fm API key. Optional: every keyless provider keeps working without it.
    pub lastfm_api_key: Option<String>,
    pub folder_template: FolderTemplate,
    pub artwork: ArtworkSettings,
    /// Automatic device sync when a device is plugged in. Opt-in, desktop only.
    pub device_sync_enabled: bool,
    /// The playback output device selected by the user, if any.
    /// `None` means use the system default audio output. This is the sink name
    /// passed to the GStreamer engine; it is validated when applied.
    pub playback_device: Option<String>,
    /// Has the user completed the first-run flow?
    pub onboarding_completed: bool,
    pub history_retention: HistoryRetention,
    pub appearance: AppearanceSettings,
    /// User mixer volume, before ReplayGain.
    pub volume: f64,
    pub muted: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            format: FormatPreference::default(),
            download_folder: None,
            cache_folder: None,
            cache_limit_bytes: DEFAULT_CACHE_LIMIT_BYTES,
            lastfm_api_key: None,
            folder_template: FolderTemplate::default(),
            artwork: ArtworkSettings::default(),
            device_sync_enabled: false,
            playback_device: None,
            onboarding_completed: false,
            history_retention: HistoryRetention::default(),
            appearance: AppearanceSettings::default(),
            volume: 1.0,
            muted: false,
        }
    }
}

impl Settings {
    /// Clamp everything into range. Called before every save, so the database
    /// only ever holds valid values.
    pub fn normalized(mut self) -> Self {
        self.volume = if self.volume.is_finite() {
            self.volume.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.format = self.format.normalized();
        self.cache_limit_bytes = self
            .cache_limit_bytes
            .clamp(MIN_CACHE_LIMIT_BYTES, u64::MAX);
        self.artwork = self.artwork.normalized();
        // A blank key means "not configured", not an empty credential.
        self.lastfm_api_key = self
            .lastfm_api_key
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty());
        if self.folder_template.as_str().trim().is_empty() {
            self.folder_template = FolderTemplate::default();
        }
        // A blank path means "use the default location", not a folder named "".
        self.download_folder = self
            .download_folder
            .take()
            .filter(|path| !path.as_os_str().is_empty());
        self.cache_folder = self
            .cache_folder
            .take()
            .filter(|path| !path.as_os_str().is_empty());
        // A blank device name means "system default", not a sink called "".
        self.playback_device = self
            .playback_device
            .map(|device| device.trim().to_string())
            .filter(|device| !device.is_empty());
        self
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Parse a stored document. Errors mean the value is unusable, in which
    /// case the caller should fall back to [`Settings::default`].
    pub fn from_json(raw: &str) -> serde_json::Result<Self> {
        serde_json::from_str::<Self>(raw).map(Settings::normalized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> TrackView {
        TrackView {
            id: 1,
            title: "Windowlicker".to_string(),
            artist: Some("Aphex Twin".to_string()),
            artist_id: Some(1),
            album: Some("Windowlicker".to_string()),
            album_id: Some(1),
            album_artist: Some("Aphex Twin".to_string()),
            track_no: Some(3),
            disc_no: Some(1),
            year: Some(1999),
            duration_ms: 360_000,
            codec: "flac".to_string(),
            bitrate_kbps: Some(900),
            file_size: 40_000_000,
            added_at: 0,
            path: PathBuf::from("/library/x.flac"),
            favourite: false,
            rating: 0,
            play_count: 0,
            replay_gain_track_db: None,
            replay_gain_album_db: None,
            replay_gain_track_peak: None,
            replay_gain_album_peak: None,
        }
    }

    #[test]
    fn track_replay_gain_wins_over_album_gain() {
        let mut view = track();
        view.replay_gain_track_db = Some(-6.5);
        view.replay_gain_album_db = Some(-3.0);
        assert_eq!(view.effective_replay_gain_db(), Some(-6.5));

        // Without a track gain the album value is the fallback.
        view.replay_gain_track_db = None;
        assert_eq!(view.effective_replay_gain_db(), Some(-3.0));

        // No gain tags at all means the engine applies no adjustment.
        view.replay_gain_album_db = None;
        assert_eq!(view.effective_replay_gain_db(), None);

        // Peaks describe the waveform, not the gain, so they never apply.
        view.replay_gain_album_peak = Some(0.5);
        assert_eq!(view.effective_replay_gain_db(), None);
    }

    #[test]
    fn blank_playback_device_means_system_default() {
        let trimmed = Settings {
            playback_device: Some("  pulsesink  ".to_string()),
            ..Settings::default()
        };
        assert_eq!(
            trimmed.normalized().playback_device.as_deref(),
            Some("pulsesink")
        );

        let blank = Settings {
            playback_device: Some("   ".to_string()),
            ..Settings::default()
        };
        assert_eq!(blank.normalized().playback_device, None);
    }

    #[test]
    fn defaults_match_the_product_brief() {
        let settings = Settings::default();
        assert_eq!(settings.format.tier, FormatTier::Lossless);
        assert_eq!(settings.format.max_bitrate_kbps, 320);
        assert_eq!(settings.cache_limit_bytes, DEFAULT_CACHE_LIMIT_BYTES);
        assert_eq!(settings.artwork.format, ArtworkFormat::Jpeg);
        assert_eq!(settings.artwork.quality, 90);
        assert!(!settings.device_sync_enabled);
        assert!(!settings.onboarding_completed);
        assert_eq!(settings.folder_template.as_str(), DEFAULT_FOLDER_TEMPLATE);
    }

    #[test]
    fn lossless_tier_prefers_lossless_but_accepts_a_ceiling_bound_fallback() {
        let preference = FormatPreference::default();
        assert!(preference.accepts(AudioFormat::Flac, None));
        assert!(preference.accepts(AudioFormat::Wav, None));
        assert!(preference.accepts(AudioFormat::Mp3, Some(320)));
        assert!(!preference.accepts(AudioFormat::Mp3, Some(500)));
        assert!(!preference.accepts(AudioFormat::Mp3, None));
        assert!(
            preference.score(AudioFormat::Flac, None)
                > preference.score(AudioFormat::Mp3, Some(320))
        );
    }

    #[test]
    fn lossy_tier_refuses_lossless_files() {
        let preference = FormatPreference::new(FormatTier::Lossy, 192);
        assert!(!preference.accepts(AudioFormat::Flac, None));
        assert!(!preference.accepts(AudioFormat::Mp3, Some(320)));
        assert!(preference.accepts(AudioFormat::Mp3, Some(192)));
        assert!(
            preference.score(AudioFormat::Mp3, Some(192))
                > preference.score(AudioFormat::Flac, None)
        );
    }

    #[test]
    fn bitrate_ceiling_is_clamped() {
        assert_eq!(
            FormatPreference::new(FormatTier::Lossy, 0).max_bitrate_kbps,
            MIN_BITRATE_KBPS
        );
        assert_eq!(
            FormatPreference::new(FormatTier::Lossy, 99_999).max_bitrate_kbps,
            MAX_BITRATE_KBPS
        );
    }

    #[test]
    fn template_renders_a_musicbee_like_path() {
        let path = FolderTemplate::default().render(&track());
        assert_eq!(
            path,
            PathBuf::from("Aphex Twin/Windowlicker/03 Windowlicker")
        );
    }

    #[test]
    fn template_handles_missing_numbers_and_unknown_placeholders() {
        let mut track = track();
        track.track_no = None;
        track.album = None;
        let template = FolderTemplate::new("{album_artist}/{album}/{track_no:02} {title}");
        assert_eq!(
            template.render(&track),
            PathBuf::from("Aphex Twin/Unknown Album/Windowlicker")
        );

        let template = FolderTemplate::new("{artist}/{mood}/{title}");
        assert_eq!(
            template.render(&track),
            PathBuf::from("Aphex Twin/{mood}/Windowlicker")
        );
    }

    #[test]
    fn template_sanitises_separators_and_traversal() {
        let mut track = track();
        track.artist = Some("AC/DC".to_string());
        track.album_artist = Some("AC/DC".to_string());
        track.album = Some("..".to_string());
        let rendered = FolderTemplate::default().render(&track);
        assert_eq!(rendered, PathBuf::from("AC_DC/03 Windowlicker"));
        assert!(!rendered.to_string_lossy().contains(".."));
    }

    #[test]
    fn template_writes_into_the_disc_folder_only_when_asked() {
        let mut track = track();
        track.disc_no = Some(2);
        let template =
            FolderTemplate::new("{album_artist}/{album}/Disc {disc_no}/{track_no:02} {title}");
        assert_eq!(
            template.render(&track),
            PathBuf::from("Aphex Twin/Windowlicker/Disc 2/03 Windowlicker")
        );
    }

    #[test]
    fn settings_survive_a_json_round_trip() {
        let settings = Settings {
            format: FormatPreference::new(FormatTier::Lossy, 256),
            download_folder: Some(PathBuf::from("/music/downloads")),
            cache_limit_bytes: 8 * 1024 * 1024 * 1024,
            artwork: ArtworkSettings {
                format: ArtworkFormat::Png,
                quality: 75,
            },
            device_sync_enabled: true,
            onboarding_completed: true,
            lastfm_api_key: Some("abc123".to_string()),
            ..Settings::default()
        };

        let restored = Settings::from_json(&settings.to_json()).unwrap();
        assert_eq!(restored, settings);
    }

    #[test]
    fn blank_folder_paths_fall_back_to_the_default_location() {
        let settings = Settings {
            download_folder: Some(PathBuf::new()),
            cache_folder: Some(PathBuf::new()),
            ..Settings::default()
        };
        let settings = settings.normalized();
        assert_eq!(settings.download_folder, None);
        assert_eq!(settings.cache_folder, None);
    }

    #[test]
    fn a_lastfm_key_is_trimmed_and_a_blank_one_disappears() {
        let settings = Settings::from_json(r#"{"lastfm_api_key": "  abc123  "}"#).unwrap();
        assert_eq!(settings.lastfm_api_key.as_deref(), Some("abc123"));

        let settings = Settings::from_json(r#"{"lastfm_api_key": "   "}"#).unwrap();
        assert_eq!(settings.lastfm_api_key, None);
    }

    #[test]
    fn stored_values_are_clamped_on_load() {
        let raw = r#"{
            "format": { "tier": "lossy", "max_bitrate_kbps": 900000 },
            "cache_limit_bytes": 1,
            "artwork": { "format": "png", "quality": 0 },
            "folder_template": "   "
        }"#;
        let settings = Settings::from_json(raw).unwrap();
        assert_eq!(settings.format.max_bitrate_kbps, MAX_BITRATE_KBPS);
        assert_eq!(settings.cache_limit_bytes, MIN_CACHE_LIMIT_BYTES);
        assert_eq!(settings.artwork.quality, 1);
        assert_eq!(settings.folder_template, FolderTemplate::default());
    }

    #[test]
    fn unknown_keys_and_missing_fields_keep_working() {
        let settings = Settings::from_json(r#"{"theme": "dark", "onboarding_completed": true}"#)
            .expect("partial documents must load");
        assert!(settings.onboarding_completed);
        assert_eq!(settings.format, FormatPreference::default());
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;

    #[test]
    fn older_settings_get_seven_days_and_full_volume() {
        let settings = Settings::from_json("{}").unwrap();
        assert_eq!(settings.history_retention, HistoryRetention::Days7);
        assert_eq!(settings.volume, 1.0);
        assert!(!settings.muted);
    }

    #[test]
    fn retention_options_round_trip_and_use_rolling_durations() {
        for (index, retention) in HistoryRetention::ALL.into_iter().enumerate() {
            assert_eq!(HistoryRetention::from_index(index), retention);
            assert_eq!(retention.index(), index);
            let settings = Settings {
                history_retention: retention,
                ..Settings::default()
            };
            assert_eq!(Settings::from_json(&settings.to_json()).unwrap(), settings);
        }
        assert_eq!(HistoryRetention::Hour.seconds(), Some(3600));
        assert_eq!(HistoryRetention::Month.seconds(), Some(30 * 86400));
        assert_eq!(HistoryRetention::Months6.seconds(), Some(180 * 86400));
        assert_eq!(HistoryRetention::Year.seconds(), Some(365 * 86400));
        assert_eq!(HistoryRetention::AllTime.seconds(), None);
    }
    #[test]
    fn appearance_defaults_are_backward_compatible() {
        let old = Settings::from_json("{}").unwrap();
        assert_eq!(old.appearance, AppearanceSettings::default());
        let partial = Settings::from_json(r#"{"appearance":{"blur_panels":false}}"#).unwrap();
        assert!(!partial.appearance.blur_panels);
        assert!(partial.appearance.cover_background);
    }
}
