//! Music domain types.

use std::path::{Path, PathBuf};

pub type TrackId = i64;
pub type AlbumId = i64;
pub type ArtistId = i64;
pub type FolderId = i64;

/// Audio containers Sonora is prepared to index and play.
///
/// Every format listed here is decodable by the GStreamer plugin set that
/// Sonora bundles (base/good/bad) — nothing here depends on the GPL `ugly`
/// plugins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioFormat {
    Flac,
    Mp3,
    Aac,
    Alac,
    Ogg,
    Opus,
    Wav,
    Aiff,
    WavPack,
    Ape,
    Musepack,
    Wma,
}

impl AudioFormat {
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "flac" => Some(Self::Flac),
            "mp3" => Some(Self::Mp3),
            "aac" => Some(Self::Aac),
            "m4a" | "alac" => Some(Self::Alac),
            "ogg" | "oga" => Some(Self::Ogg),
            "opus" => Some(Self::Opus),
            "wav" | "wave" => Some(Self::Wav),
            "aiff" | "aif" | "aifc" => Some(Self::Aiff),
            "wv" => Some(Self::WavPack),
            "ape" => Some(Self::Ape),
            "mpc" => Some(Self::Musepack),
            "wma" => Some(Self::Wma),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::Mp3 => "mp3",
            Self::Aac => "aac",
            Self::Alac => "m4a",
            Self::Ogg => "ogg",
            Self::Opus => "opus",
            Self::Wav => "wav",
            Self::Aiff => "aiff",
            Self::WavPack => "wv",
            Self::Ape => "ape",
            Self::Musepack => "mpc",
            Self::Wma => "wma",
        }
    }

    /// Is this a lossless container? Used by the format preference to rank
    /// candidates and by the cache to know what a bitrate ceiling applies to.
    pub fn is_lossless(self) -> bool {
        matches!(
            self,
            Self::Flac | Self::Alac | Self::Wav | Self::Aiff | Self::WavPack | Self::Ape
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Flac => "FLAC",
            Self::Mp3 => "MP3",
            Self::Aac => "AAC",
            Self::Alac => "ALAC",
            Self::Ogg => "Vorbis",
            Self::Opus => "Opus",
            Self::Wav => "WAV",
            Self::Aiff => "AIFF",
            Self::WavPack => "WavPack",
            Self::Ape => "Monkey's Audio",
            Self::Musepack => "Musepack",
            Self::Wma => "WMA",
        }
    }
}

/// Is this path a file Sonora should try to index?
pub fn is_supported_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .and_then(AudioFormat::from_extension)
        .is_some()
}

/// A track after metadata extraction, before it is written to the database.
#[derive(Debug, Clone)]
pub struct ScannedTrack {
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub genres: Vec<String>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub year: Option<i32>,
    pub duration_ms: u64,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    pub codec: String,
    pub file_size: u64,
    pub modified_at: i64,
}

impl ScannedTrack {
    /// Album artist falls back to the track artist, which is what users expect
    /// for single-artist albums and what keeps the album grid tidy.
    pub fn effective_album_artist(&self) -> Option<&str> {
        self.album_artist.as_deref().or(self.artist.as_deref())
    }

    pub fn effective_album(&self) -> &str {
        self.album.as_deref().unwrap_or("Unknown Album")
    }
}

/// A row ready for display: names are already joined, so the UI never has to
/// resolve ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackView {
    pub id: TrackId,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_no: Option<i64>,
    pub disc_no: Option<i64>,
    pub year: Option<i64>,
    pub duration_ms: i64,
    pub codec: String,
    pub bitrate_kbps: Option<i64>,
    pub file_size: i64,
    pub added_at: i64,
    pub path: PathBuf,
    /// Has the user starred this track?
    pub favourite: bool,
    /// 0 means unrated; 1–5 are the stars the user chose.
    pub rating: i64,
    /// How many times playback has started on this track.
    pub play_count: i64,
}

impl TrackView {
    pub fn artist_or_unknown(&self) -> &str {
        self.artist.as_deref().unwrap_or("Unknown Artist")
    }

    pub fn album_or_unknown(&self) -> &str {
        self.album.as_deref().unwrap_or("Unknown Album")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LibraryStats {
    pub tracks: i64,
    pub albums: i64,
    pub artists: i64,
    pub genres: i64,
    pub folders: i64,
    pub total_duration_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanError {
    pub path: PathBuf,
    pub message: String,
}

/// Outcome of a scan, suitable for both the CLI and the UI.
#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub folder: PathBuf,
    pub seen: u64,
    pub imported: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub removed: u64,
    pub errors: Vec<ScanError>,
    pub elapsed_ms: u128,
}

impl ScanReport {
    pub fn total_errors(&self) -> u64 {
        self.errors.len() as u64
    }
}
