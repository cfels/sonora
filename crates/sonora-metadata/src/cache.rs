//! Small on-disk cache for provider responses.
//!
//! Lyrics are stored per track in the database, but artwork lookups and Last.fm
//! tags are keyed by request, so they live here: one hashed file per key, with a
//! time-to-live so a wrong match can still be corrected later.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sonora_core::{Error, Result};

pub struct DiskCache {
    root: PathBuf,
    ttl: Duration,
}

impl DiskCache {
    pub fn at(root: impl Into<PathBuf>, ttl: Duration) -> Self {
        Self {
            root: root.into(),
            ttl,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, key: &str) -> PathBuf {
        let digest = blake3::hash(key.as_bytes()).to_hex();
        self.root.join(format!("{digest}.bin"))
    }

    /// Read a cached entry, dropping it when it outlived the TTL.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let path = self.path_for(key);
        let metadata = std::fs::metadata(&path).ok()?;
        let age = metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .unwrap_or_default();
        if age > self.ttl {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        std::fs::read(&path).ok()
    }

    pub fn put(&self, key: &str, value: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.root).map_err(|err| Error::io(&self.root, err))?;
        std::fs::write(self.path_for(key), value).map_err(|err| Error::io(&self.root, err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_value() {
        let directory = tempfile::tempdir().unwrap();
        let cache = DiskCache::at(directory.path(), Duration::from_secs(3600));
        assert_eq!(cache.get("https://example.test/a"), None);

        cache.put("https://example.test/a", b"payload").unwrap();
        assert_eq!(
            cache.get("https://example.test/a"),
            Some(b"payload".to_vec())
        );
    }

    #[test]
    fn keys_are_hashed_so_urls_never_become_file_names() {
        let directory = tempfile::tempdir().unwrap();
        let cache = DiskCache::at(directory.path(), Duration::from_secs(3600));
        cache
            .put("https://example.test/a?b=../../etc/passwd", b"x")
            .unwrap();

        let entries: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].ends_with(".bin"));
        assert!(!entries[0].contains(".."), "{entries:?}");
    }

    #[test]
    fn a_zero_ttl_expires_immediately() {
        let directory = tempfile::tempdir().unwrap();
        let cache = DiskCache::at(directory.path(), Duration::ZERO);
        cache.put("key", b"value").unwrap();
        // Freshly written files can share the write timestamp; a zero TTL means
        // anything older than "now" is stale, so sleep past the clock tick.
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(cache.get("key"), None);
    }
}
