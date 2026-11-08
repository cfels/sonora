//! LRU cache for tracks Sonora fetched instead of finding in the library.
//!
//! The rules are the ones in `docs/REQUIREMENTS.md`:
//!
//! * a size budget the listener sets,
//! * least-recently-played files are evicted first,
//! * eviction also runs when free disk space falls below a safety floor,
//! * the playing track and everything queued behind it are never evicted,
//! * every eviction is logged so the user can see what was dropped.
//!
//! Recency is tracked in a small `index.json` beside the files rather than in
//! file timestamps: reading a file is not the same as playing it, and copying a
//! cache directory must not silently reset the order.
//!
//! The eviction decision itself ([`plan_eviction`]) is a pure function over
//! entries, so the policy is tested without touching a disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sonora_core::{Error, Result, TrackId};

/// Free space that must remain on the cache volume, whether or not the budget
/// is full. Evicting a few files is always cheaper than filling the disk.
pub const DEFAULT_FREE_SPACE_FLOOR: u64 = 512 * 1024 * 1024;

const INDEX_FILE: &str = "index.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub track: TrackId,
    pub path: PathBuf,
    pub size: u64,
    /// Unix seconds of the last playback, or the file's modification time when
    /// the index has no record of it.
    pub last_played: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub tracks: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvictionReport {
    pub removed: Vec<TrackId>,
    pub freed_bytes: u64,
    pub kept_bytes: u64,
    pub kept_tracks: usize,
}

#[derive(Debug, Default)]
pub struct ClearReport {
    pub removed: usize,
    pub freed_bytes: u64,
    pub kept: usize,
    pub errors: Vec<String>,
}

impl EvictionReport {
    pub fn removed_count(&self) -> usize {
        self.removed.len()
    }
}

/// Which tracks have to go to satisfy the budget and the free-space floor.
///
/// `protected` is the playing track plus the queue behind it. Those are skipped
/// even when they are the oldest entries, which means a queue larger than the
/// budget can leave the cache over its limit for a while — playing music is
/// never traded for a number in a settings screen.
pub fn plan_eviction(
    entries: &[CacheEntry],
    limit_bytes: u64,
    available_bytes: Option<u64>,
    floor_bytes: u64,
    protected: &[TrackId],
) -> Vec<TrackId> {
    let mut oldest_first: Vec<&CacheEntry> = entries.iter().collect();
    oldest_first.sort_by_key(|entry| (entry.last_played, entry.track));

    let mut remaining: u64 = entries.iter().map(|entry| entry.size).sum();
    let mut available = available_bytes;
    let mut evicted = Vec::new();

    for entry in oldest_first {
        let over_budget = remaining > limit_bytes;
        let below_floor = matches!(available, Some(free) if floor_bytes > 0 && free < floor_bytes);
        if !over_budget && !below_floor {
            break;
        }
        if protected.contains(&entry.track) {
            continue;
        }

        evicted.push(entry.track);
        remaining = remaining.saturating_sub(entry.size);
        if let Some(free) = available.as_mut() {
            *free = free.saturating_add(entry.size);
        }
    }

    evicted
}

/// The cache directory and its policy.
pub struct TrackCache {
    root: PathBuf,
    limit_bytes: u64,
    floor_bytes: u64,
}

impl TrackCache {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self::with_policy(
            root,
            sonora_core::settings::DEFAULT_CACHE_LIMIT_BYTES,
            DEFAULT_FREE_SPACE_FLOOR,
        )
    }

    pub fn with_policy(root: impl Into<PathBuf>, limit_bytes: u64, floor_bytes: u64) -> Self {
        Self {
            root: root.into(),
            limit_bytes,
            floor_bytes,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }

    pub fn floor_bytes(&self) -> u64 {
        self.floor_bytes
    }

    /// File name for a cached track: the id plus the container extension.
    pub fn path_for(&self, track: TrackId, extension: &str) -> PathBuf {
        self.root
            .join(format!("{track}.{}", clean_extension(extension)))
    }

    /// Everything the cache currently holds, oldest played first.
    pub fn entries(&self) -> Result<Vec<CacheEntry>> {
        let index = self.load_index()?;
        let directory = match std::fs::read_dir(&self.root) {
            Ok(directory) => directory,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(Error::io(&self.root, err)),
        };

        let mut entries = Vec::new();
        for item in directory {
            let item = item.map_err(|err| Error::io(&self.root, err))?;
            let path = item.path();
            if !item
                .file_type()
                .map_err(|err| Error::io(&path, err))?
                .is_file()
            {
                continue;
            }
            let Some(track) = parse_track_id(&path) else {
                continue;
            };
            let metadata = item.metadata().map_err(|err| Error::io(&path, err))?;
            let last_played = index
                .played
                .get(&track.to_string())
                .copied()
                .unwrap_or_else(|| modified_seconds(&metadata));
            entries.push(CacheEntry {
                track,
                path,
                size: metadata.len(),
                last_played,
            });
        }

        entries.sort_by_key(|entry| (entry.last_played, entry.track));
        Ok(entries)
    }

    pub fn stats(&self) -> Result<CacheStats> {
        let entries = self.entries()?;
        Ok(CacheStats {
            tracks: entries.len(),
            bytes: entries.iter().map(|entry| entry.size).sum(),
        })
    }

    /// Add a fetched track. Replacing an existing file also marks it recent.
    pub fn put(&self, track: TrackId, extension: &str, bytes: &[u8]) -> Result<CacheEntry> {
        std::fs::create_dir_all(&self.root).map_err(|err| Error::io(&self.root, err))?;
        let path = self.path_for(track, extension);
        std::fs::write(&path, bytes).map_err(|err| Error::io(&path, err))?;
        self.touch(track)?;

        Ok(CacheEntry {
            track,
            path,
            size: bytes.len() as u64,
            last_played: now(),
        })
    }

    /// Record a playback without copying anything.
    pub fn touch(&self, track: TrackId) -> Result<()> {
        self.touch_at(track, now())
    }

    pub fn touch_at(&self, track: TrackId, unix_seconds: u64) -> Result<()> {
        let mut index = self.load_index()?;
        index.played.insert(track.to_string(), unix_seconds);
        self.save_index(&index)
    }

    pub fn remove(&self, track: TrackId) -> Result<bool> {
        let entries: Vec<CacheEntry> = self
            .entries()?
            .into_iter()
            .filter(|entry| entry.track == track)
            .collect();
        if entries.is_empty() {
            return Ok(false);
        }
        for entry in entries {
            std::fs::remove_file(&entry.path).map_err(|err| Error::io(&entry.path, err))?;
        }
        let mut index = self.load_index()?;
        index.played.remove(&track.to_string());
        self.save_index(&index)?;
        Ok(true)
    }

    /// Clear only files registered by this cache. Never traverse directories,
    /// follow symlinks, remove library originals or disturb playing/queued files.
    /// A damaged index leaves unowned files alone rather than guessing ownership.
    pub fn clear(&self, protected: &[TrackId], originals: &[PathBuf]) -> Result<ClearReport> {
        let mut index = self.load_index()?;
        let mut report = ClearReport::default();
        let originals: Vec<PathBuf> = originals
            .iter()
            .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
            .collect();
        for entry in self.entries()? {
            let metadata = std::fs::symlink_metadata(&entry.path)
                .map_err(|err| Error::io(&entry.path, err))?;
            let canonical = entry
                .path
                .canonicalize()
                .map_err(|err| Error::io(&entry.path, err))?;
            if protected.contains(&entry.track)
                || !index.played.contains_key(&entry.track.to_string())
                || !metadata.file_type().is_file()
                || originals.contains(&canonical)
            {
                report.kept += 1;
                continue;
            }
            match std::fs::remove_file(&entry.path) {
                Ok(()) => {
                    report.removed += 1;
                    report.freed_bytes += entry.size;
                    // A track can have more than one cached encoding. Keep the
                    // ownership entry until all of its files were considered.
                }
                Err(err) => report
                    .errors
                    .push(format!("{}: {err}", entry.path.display())),
            }
        }
        if report.removed > 0 {
            let remaining = self.entries()?;
            index
                .played
                .retain(|id, _| remaining.iter().any(|entry| entry.track.to_string() == *id));
            self.save_index(&index)?;
        }
        Ok(report)
    }

    /// Enforce the budget against the real disk.
    pub fn enforce(&self, protected: &[TrackId]) -> Result<EvictionReport> {
        self.enforce_with_available(protected, free_space(&self.root))
    }

    /// Enforce the budget against a caller-supplied free-space figure.
    ///
    /// Splitting this out keeps the floor behaviour testable: a CI machine's
    /// free space is not something a test can set up.
    pub fn enforce_with_available(
        &self,
        protected: &[TrackId],
        available_bytes: Option<u64>,
    ) -> Result<EvictionReport> {
        let entries = self.entries()?;
        let doomed = plan_eviction(
            &entries,
            self.limit_bytes,
            available_bytes,
            self.floor_bytes,
            protected,
        );

        let mut report = EvictionReport::default();
        for entry in &entries {
            if !doomed.contains(&entry.track) {
                continue;
            }
            match std::fs::remove_file(&entry.path) {
                Ok(()) => {
                    log::info!(
                        "cache: evicted track {} ({} bytes) from {}",
                        entry.track,
                        entry.size,
                        entry.path.display()
                    );
                    report.removed.push(entry.track);
                    report.freed_bytes += entry.size;
                }
                Err(err) => log::warn!("cache: could not remove {}: {err}", entry.path.display()),
            }
        }

        let removed: Vec<TrackId> = report.removed.clone();
        if !removed.is_empty() {
            let mut index = self.load_index()?;
            for track in removed {
                index.played.remove(&track.to_string());
            }
            self.save_index(&index)?;
        }

        let remaining: Vec<&CacheEntry> = entries
            .iter()
            .filter(|entry| !report.removed.contains(&entry.track))
            .collect();
        report.kept_tracks = remaining.len();
        report.kept_bytes = remaining.iter().map(|entry| entry.size).sum();
        Ok(report)
    }

    fn load_index(&self) -> Result<Index> {
        let path = self.root.join(INDEX_FILE);
        match std::fs::read(&path) {
            Ok(raw) => Ok(serde_json::from_slice(&raw).unwrap_or_else(|err| {
                log::warn!("cache: ignoring unreadable index {}: {err}", path.display());
                Index::default()
            })),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Index::default()),
            Err(err) => Err(Error::io(&path, err)),
        }
    }

    fn save_index(&self, index: &Index) -> Result<()> {
        std::fs::create_dir_all(&self.root).map_err(|err| Error::io(&self.root, err))?;
        let path = self.root.join(INDEX_FILE);
        let temporary = self.root.join(format!("{INDEX_FILE}.tmp"));
        let document = serde_json::to_vec(index).map_err(|err| {
            Error::io(
                &path,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("could not encode the cache index: {err}"),
                ),
            )
        })?;
        std::fs::write(&temporary, document).map_err(|err| Error::io(&temporary, err))?;
        std::fs::rename(&temporary, &path).map_err(|err| Error::io(&path, err))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    played: HashMap<String, u64>,
}

fn parse_track_id(path: &Path) -> Option<TrackId> {
    let stem = path.file_stem()?.to_str()?;
    stem.parse::<TrackId>().ok().filter(|id| *id > 0)
}

fn clean_extension(extension: &str) -> String {
    let cleaned: String = extension
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    if cleaned.is_empty() {
        "bin".to_string()
    } else {
        cleaned
    }
}

fn modified_seconds(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|age| age.as_secs())
        .unwrap_or_default()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|age| age.as_secs())
        .unwrap_or_default()
}

/// Free space available to the current user on the volume holding `path`.
///
/// Returns `None` when the platform cannot answer, in which case eviction is
/// driven by the size budget alone.
#[cfg(unix)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `stats` is a zeroed struct we own and `c_path` is NUL-terminated.
    unsafe {
        let mut stats: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stats) != 0 {
            return None;
        }
        Some(stats.f_bavail as u64 * stats.f_frsize as u64)
    }
}

#[cfg(windows)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available: u64 = 0;
    // SAFETY: the path is NUL-terminated and `available` is a valid out pointer.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available)
}

#[cfg(not(any(unix, windows)))]
pub fn free_space(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(track: TrackId, size: u64, last_played: u64) -> CacheEntry {
        CacheEntry {
            track,
            path: PathBuf::from(format!("/cache/{track}.flac")),
            size,
            last_played,
        }
    }

    #[test]
    fn nothing_is_evicted_when_everything_fits() {
        let entries = vec![entry(1, 100, 10), entry(2, 100, 20)];
        assert!(plan_eviction(&entries, 1_000, Some(10_000), 500, &[]).is_empty());
    }

    #[test]
    fn the_least_recently_played_track_goes_first() {
        let entries = vec![entry(1, 400, 30), entry(2, 400, 10), entry(3, 400, 20)];
        // 1 200 bytes against a 900 byte budget: one eviction is enough.
        let evicted = plan_eviction(&entries, 900, None, 0, &[]);
        assert_eq!(evicted, vec![2]);
    }

    #[test]
    fn the_budget_is_met_even_when_several_tracks_have_to_go() {
        let entries = vec![entry(1, 500, 10), entry(2, 500, 20), entry(3, 500, 30)];
        assert_eq!(plan_eviction(&entries, 600, None, 0, &[]), vec![1, 2]);
    }

    #[test]
    fn the_playing_track_and_queue_are_never_evicted() {
        let entries = vec![entry(1, 500, 10), entry(2, 500, 20), entry(3, 500, 30)];
        let evicted = plan_eviction(&entries, 0, None, 0, &[1, 2]);
        assert_eq!(evicted, vec![3]);
    }

    #[test]
    fn a_queue_larger_than_the_budget_leaves_the_cache_over_its_limit() {
        let entries = vec![entry(1, 500, 10), entry(2, 500, 20)];
        assert!(plan_eviction(&entries, 0, None, 0, &[1, 2]).is_empty());
    }

    #[test]
    fn a_low_disk_triggers_eviction_even_under_budget() {
        let entries = vec![entry(1, 1_000, 10), entry(2, 1_000, 20)];
        // 200 bytes free against a 1 500 byte floor: the oldest frees 1 000, the
        // next one is needed to climb past the floor.
        let evicted = plan_eviction(&entries, 10_000, Some(200), 1_500, &[]);
        assert_eq!(evicted, vec![1, 2]);
    }

    #[test]
    fn a_low_disk_keeps_protected_tracks_until_the_floor_is_met() {
        let entries = vec![entry(1, 1_000, 10), entry(2, 1_000, 20)];
        let evicted = plan_eviction(&entries, 10_000, Some(200), 1_500, &[1]);
        assert_eq!(evicted, vec![2]);
    }

    #[test]
    fn unknown_free_space_falls_back_to_the_budget_alone() {
        let entries = vec![entry(1, 1_000, 10)];
        assert!(plan_eviction(&entries, 10_000, None, u64::MAX, &[]).is_empty());
    }

    #[test]
    fn putting_and_touching_tracks_tracks_recency() {
        let directory = tempfile::tempdir().unwrap();
        let cache = TrackCache::with_policy(directory.path(), 10_000, 0);

        cache.put(1, "flac", b"aaaa").unwrap();
        cache.put(2, "mp3", b"bbbb").unwrap();
        cache.touch_at(1, 5_000).unwrap();

        let entries = cache.entries().unwrap();
        assert_eq!(entries.len(), 2);
        // Both were written "now"; the explicit touch makes track 1 older than
        // the file it arrived with, which is the whole point of the index.
        assert_eq!(entries[0].track, 1, "track 1 was touched at 1970");
        assert_eq!(entries[0].last_played, 5_000);
        assert_eq!(entries[0].size, 4);
        assert!(entries[0].path.ends_with("1.flac"));
        assert_eq!(entries[1].track, 2);
        assert!(entries[1].last_played > 5_000);
    }

    #[test]
    fn extensions_are_sanitised_so_a_cache_path_cannot_escape() {
        let cache = TrackCache::at("/cache");
        assert_eq!(
            cache.path_for(7, "../../etc/passwd"),
            PathBuf::from("/cache/7.etcpassw")
        );
        assert_eq!(cache.path_for(7, ""), PathBuf::from("/cache/7.bin"));
    }

    #[test]
    fn removing_a_track_deletes_the_file_and_its_history() {
        let directory = tempfile::tempdir().unwrap();
        let cache = TrackCache::with_policy(directory.path(), 10_000, 0);
        cache.put(1, "flac", b"aaaa").unwrap();
        cache.put(2, "flac", b"bbbb").unwrap();

        assert!(cache.remove(1).unwrap());
        assert!(!cache.remove(1).unwrap());
        assert_eq!(
            cache
                .entries()
                .unwrap()
                .iter()
                .map(|e| e.track)
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn enforcing_the_budget_deletes_the_oldest_file_and_reports_it() {
        let directory = tempfile::tempdir().unwrap();
        let cache = TrackCache::with_policy(directory.path(), 6, 0);

        cache.put(1, "flac", b"aaaa").unwrap();
        cache.put(2, "flac", b"bbbb").unwrap();
        cache.touch_at(1, 9_000).unwrap();
        cache.touch_at(2, 1_000).unwrap();

        let report = cache.enforce_with_available(&[], None).unwrap();
        assert_eq!(report.removed, vec![2]);
        assert_eq!(report.freed_bytes, 4);
        assert_eq!(report.kept_tracks, 1);
        assert_eq!(report.kept_bytes, 4);
        assert!(!directory.path().join("2.flac").exists());
        assert!(directory.path().join("1.flac").exists());
        assert_eq!(cache.stats().unwrap().tracks, 1);
    }

    #[test]
    fn enforcement_protects_the_playing_track() {
        let directory = tempfile::tempdir().unwrap();
        let cache = TrackCache::with_policy(directory.path(), 0, 0);
        cache.put(1, "flac", b"aaaa").unwrap();
        cache.put(2, "flac", b"bbbb").unwrap();

        let report = cache.enforce_with_available(&[1], None).unwrap();
        assert_eq!(report.removed, vec![2]);
        assert!(directory.path().join("1.flac").exists());
    }

    #[test]
    fn unrelated_files_are_left_alone() {
        let directory = tempfile::tempdir().unwrap();
        let cache = TrackCache::with_policy(directory.path(), 1, 0);
        std::fs::write(directory.path().join("notes.txt"), b"not a track").unwrap();
        std::fs::write(directory.path().join("cover.jpg"), b"art").unwrap();
        cache.put(3, "flac", b"cccc").unwrap();
        cache.touch_at(3, 1).unwrap();

        let report = cache.enforce_with_available(&[], None).unwrap();
        assert_eq!(report.removed, vec![3]);
        assert!(directory.path().join("notes.txt").exists());
        assert!(directory.path().join("cover.jpg").exists());
    }

    #[test]
    fn a_corrupt_index_does_not_stop_the_cache_from_working() {
        let directory = tempfile::tempdir().unwrap();
        let cache = TrackCache::with_policy(directory.path(), 10_000, 0);
        cache.put(1, "flac", b"aaaa").unwrap();
        std::fs::write(directory.path().join(INDEX_FILE), b"{ not json").unwrap();

        let entries = cache.entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].track, 1);
    }

    #[test]
    fn free_space_is_reported_for_a_real_directory() {
        let directory = tempfile::tempdir().unwrap();
        let available = free_space(directory.path()).expect("free space on a temp dir");
        assert!(available > 0);
        assert!(free_space(Path::new("/definitely/not/a/path")).is_none());
    }
    #[test]
    fn manual_clear_keeps_unowned_protected_and_library_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TrackCache::at(dir.path());
        for id in 1..=4 {
            cache.put(id, "flac", b"music").unwrap();
        }
        cache.put(1, "ogg", b"other encoding").unwrap();
        let original = cache.path_for(4, "flac");
        std::fs::write(dir.path().join("5.flac"), b"unowned").unwrap();
        std::fs::write(dir.path().join("library.db"), b"metadata").unwrap();
        let report = cache
            .clear(&[2, 3], std::slice::from_ref(&original))
            .unwrap();
        assert_eq!(report.removed, 2);
        assert!(report.errors.is_empty());
        for name in ["2.flac", "3.flac", "4.flac", "5.flac", "library.db"] {
            assert!(dir.path().join(name).exists());
        }
        assert!(!cache.path_for(1, "flac").exists());
        assert!(!cache.path_for(1, "ogg").exists());
        assert!(cache.clear(&[], &[original]).unwrap().errors.is_empty());
    }

    #[test]
    fn damaged_cache_index_never_authorizes_manual_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TrackCache::at(dir.path());
        cache.put(1, "flac", b"music").unwrap();
        std::fs::write(dir.path().join(INDEX_FILE), b"broken").unwrap();
        assert_eq!(cache.clear(&[], &[]).unwrap().removed, 0);
        assert!(cache.path_for(1, "flac").exists());
    }

    #[test]
    fn removing_a_track_removes_all_of_its_cached_encodings() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TrackCache::at(dir.path());
        cache.put(1, "flac", b"one").unwrap();
        cache.put(1, "mp3", b"two").unwrap();
        assert!(cache.remove(1).unwrap());
        assert_eq!(cache.stats().unwrap().tracks, 0);
    }
}
