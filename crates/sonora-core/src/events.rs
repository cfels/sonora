//! Engine → UI messages.
//!
//! Long-running work (scanning now; playback, downloads and device transfers
//! later) publishes [`Event`]s on a broadcast channel. The UI never blocks on
//! a scan: it keeps rendering and simply receives progress.

use std::path::PathBuf;

use crate::model::{LibraryStats, ScanReport};

#[derive(Debug, Clone, Default)]
pub struct ScanProgress {
    pub folder: PathBuf,
    pub seen: u64,
    pub imported: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub errors: u64,
    pub current: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum Event {
    ScanStarted {
        folder: PathBuf,
    },
    ScanProgress(ScanProgress),
    ScanFinished(ScanReport),
    LibraryChanged(LibraryStats),
    /// Human-readable problem worth surfacing in the status bar.
    Warn(String),
}
