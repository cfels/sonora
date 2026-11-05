//! Domain types shared by every Sonora crate.
//!
//! `sonora-core` deliberately has almost no dependencies: it describes the
//! music domain (tracks, albums, artists, scan results) and the messages that
//! travel between the UI and the engine. Storage, playback, network services
//! and device sync all depend on this crate, never on each other.

pub mod error;
pub mod events;
pub mod model;
pub mod queue;
pub mod settings;
pub mod util;

pub use error::{Error, Result};
pub use events::{Event, ScanProgress};
pub use model::{
    is_supported_audio, AlbumId, ArtistId, AudioFormat, FolderId, LibraryStats, ScanError,
    ScanReport, ScannedTrack, TrackId, TrackView,
};
pub use queue::{Queue, QueueSnapshot, RepeatMode};
pub use settings::{
    ArtworkFormat, ArtworkSettings, FolderTemplate, FormatPreference, FormatTier, Settings,
    DEFAULT_FOLDER_TEMPLATE, QUEUE_KEY, SETTINGS_KEY,
};
pub use util::{
    format_duration, new_rng, normalize_key, parse_artist_title, sort_key, without_extension,
};
