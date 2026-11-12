//! M-07: quarantine for untrusted bytes.
//!
//! When a replica pulls a chunk from an upstream and the decrypted
//! plaintext does not hash to the chunk reference's `plain_hash`, the
//! bytes are hostile (compromised primary, MITM, bit corruption) and must
//! NEVER reach the CAS blob path or the L2 cache. They are written here —
//! `<quarantine_dir>/<hash>.bin` — content-addressed so duplicates dedupe,
//! size-bounded with oldest-mtime eviction, and swept at boot when older
//! than `storage.quarantine_retention_secs`.
//!
//! The quarantine directory is an ops surface, not user data: it is NOT
//! subject to tenant quota, and it lives OUTSIDE the CAS walk so GC /
//! reconcile never see quarantined bytes as blobs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::StorageError;

pub struct Quarantine {
    dir: PathBuf,
    max_bytes: u64,
}

impl Quarantine {
    pub fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self { dir, max_bytes }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Persists the hostile bytes under their content hash, then enforces
    /// the directory bound (oldest mtime evicted first). Returns the
    /// on-disk size. A quarantine write failure is logged by the caller
    /// but never masks the `Corruption` error that triggered it.
    pub async fn store(&self, hash: &[u8; 32], bytes: &[u8]) -> Result<u64, StorageError> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(StorageError::from)?;
        let path = self.path_for(hash);
        // Content-addressed: same hostile hash → same file, rewrite is a
        // no-op semantically. Atomic-ish: write to a tmp name then rename.
        let tmp = self
            .dir
            .join(format!(".qtmp-{}", uuid::Uuid::now_v7().simple()));
        tokio::fs::write(&tmp, bytes)
            .await
            .map_err(StorageError::from)?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(StorageError::from)?;
        let size = bytes.len() as u64;
        self.enforce_bound().await?;
        Ok(size)
    }

    fn path_for(&self, hash: &[u8; 32]) -> PathBuf {
        self.dir.join(format!("{}.bin", hex::encode(hash)))
    }

    /// Evicts oldest-mtime files while the directory holds more than
    /// `max_bytes` bytes. Returns the number of files evicted.
    async fn enforce_bound(&self) -> Result<u64, StorageError> {
        let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
        let mut rd = match tokio::fs::read_dir(&self.dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(StorageError::from(e)),
        };
        while let Some(entry) = rd.next_entry().await.map_err(StorageError::from)? {
            let path = entry.path();
            if path.is_dir() {
                continue;
            }
            let name_ok = path.extension().map(|x| x == "bin").unwrap_or(false);
            if !name_ok {
                continue; // never touch non-quarantine files (or our tmp files)
            }
            let meta = entry.metadata().await.map_err(StorageError::from)?;
            let modified = meta.modified().unwrap_or(std::time::SystemTime::now());
            entries.push((path, meta.len(), modified));
        }
        let total: u64 = entries.iter().map(|(_, len, _)| *len).sum();
        if total <= self.max_bytes {
            return Ok(0);
        }
        entries.sort_by_key(|(_, _, mtime)| *mtime);
        let mut evicted = 0u64;
        let mut total = total;
        for (path, len, _) in &entries {
            if total <= self.max_bytes {
                break;
            }
            if tokio::fs::remove_file(path).await.is_ok() {
                total = total.saturating_sub(*len);
                evicted += 1;
            }
        }
        Ok(evicted)
    }

    /// Boot sweep: removes quarantine files older than `retention`.
    /// Returns the number removed (metric: `startup_quarantine_swept`).
    pub async fn sweep_expired(&self, retention: Duration) -> Result<u64, StorageError> {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .saturating_sub(retention);
        let mut removed = 0u64;
        let mut rd = match tokio::fs::read_dir(&self.dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(StorageError::from(e)),
        };
        while let Some(entry) = rd.next_entry().await.map_err(StorageError::from)? {
            let path = entry.path();
            if path.is_dir() {
                continue;
            }
            let meta = entry.metadata().await.map_err(StorageError::from)?;
            let age_ok = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                .map(|d| d < cutoff)
                .unwrap_or(false);
            if age_ok && tokio::fs::remove_file(&path).await.is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Whether a quarantine entry exists for `hash` (test/gate helper).
    pub async fn contains(&self, hash: &[u8; 32]) -> bool {
        tokio::fs::try_exists(self.path_for(hash))
            .await
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(dir: &Path, max: u64) -> Quarantine {
        Quarantine::new(dir.to_path_buf(), max)
    }

    fn hash(n: u8) -> [u8; 32] {
        let mut h = [0u8; 32];
        h[0] = n;
        h[1] = 0xA5;
        h
    }

    #[tokio::test]
    async fn stores_and_reports_containment() {
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let q = q(dir.path(), 1 << 20);
        q.store(&hash(1), b"hostile").await.unwrap();
        assert!(q.contains(&hash(1)).await);
        assert!(!q.contains(&hash(2)).await);
        let on_disk =
            std::fs::read(dir.path().join(format!("{}.bin", hex::encode(hash(1))))).unwrap();
        assert_eq!(on_disk, b"hostile");
    }

    #[tokio::test]
    async fn oldest_evicted_on_bound() {
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let q = q(dir.path(), 150);
        // 3 files of 100 bytes each; bound 150 → oldest two evicted.
        for i in 0..3u8 {
            q.store(&hash(i), &[i; 100]).await.unwrap();
            // Ensure strictly increasing mtimes (some filesystems have
            // 1s granularity).
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!q.contains(&hash(0)).await, "oldest evicted");
        assert!(!q.contains(&hash(1)).await, "second-oldest evicted");
        assert!(q.contains(&hash(2)).await, "newest survives");
    }

    #[tokio::test]
    async fn sweep_expired_removes_only_old() {
        use std::fs::FileTimes;
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let q = q(dir.path(), 1 << 20);
        q.store(&hash(1), b"old").await.unwrap();
        // Age the file backwards beyond the retention window.
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        let path = dir.path().join(format!("{}.bin", hex::encode(hash(1))));
        let f = std::fs::File::options().append(true).open(&path).unwrap();
        let times = FileTimes::new().set_accessed(old).set_modified(old);
        f.set_times(times).unwrap();
        drop(f);
        q.store(&hash(2), b"fresh").await.unwrap();
        let removed = q.sweep_expired(Duration::from_secs(60)).await.unwrap();
        assert_eq!(removed, 1);
        assert!(!q.contains(&hash(1)).await);
        assert!(q.contains(&hash(2)).await);
    }
}
