//! Content-addressed blob store on the local filesystem.
//!
//! Layout: `<root>/<ab>/<cd>/<64-hex>.bin` where `ab`/`cd` are the first
//! `shard_depth` bytes of the hash (hex). Writes are atomic:
//! write to a unique temp file -> fsync file -> rename -> fsync directory.
//! Concurrent writers of the same hash therefore converge idempotently.

use bytes::Bytes;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::error::{dbe, not_found, StorageError};

pub struct BlockStore {
    root: PathBuf,
    shard_depth: u32,
}

impl BlockStore {
    pub fn new(root: impl Into<PathBuf>, shard_depth: u32) -> Self {
        Self {
            root: root.into(),
            shard_depth,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `(directory, final_path)` for a hash.
    pub fn shard_path(&self, hash: &[u8; 32]) -> (PathBuf, PathBuf) {
        let hex = hex::encode(hash);
        let mut dir = self.root.clone();
        let take_bytes = (self.shard_depth as usize).min(31);
        for i in 0..take_bytes {
            dir.push(&hex[i * 2..i * 2 + 2]);
        }
        let mut file = dir.clone();
        file.push(format!("{hex}.bin"));
        (dir, file)
    }

    /// Relative path (`ab/cd/<hex>.bin`) recorded in chunk metadata.
    pub fn relative_path(&self, hash: &[u8; 32]) -> String {
        let hex = hex::encode(hash);
        let take_bytes = (self.shard_depth as usize).min(31);
        let mut parts: Vec<String> = Vec::with_capacity(self.shard_depth as usize + 1);
        for i in 0..take_bytes {
            parts.push(hex[i * 2..i * 2 + 2].to_string());
        }
        parts.push(format!("{hex}.bin"));
        parts.join("/")
    }

    /// Resolves a relative path recorded in metadata.
    pub fn resolve_relative(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Atomically writes a blob. Returns `true` if the blob was written,
    /// `false` when it already existed (idempotent).
    /// C-03: synchronous presence check for a relative blob path. Used
    /// INSIDE the verified-registration transaction (runs on the blocking
    /// pool, so a stat syscall here is fine).
    pub fn path_exists(&self, relative: &str) -> bool {
        std::fs::metadata(self.root.join(relative)).is_ok()
    }

    pub async fn write(&self, hash: &[u8; 32], data: &[u8]) -> Result<bool, StorageError> {
        let (dir, path) = self.shard_path(hash);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(false);
        }
        tokio::fs::create_dir_all(&dir).await.map_err(dbe)?;
        let tmp = dir.join(format!(".tmp-{}", Uuid::now_v7().as_simple()));
        {
            let mut f = tokio::fs::File::create(&tmp).await.map_err(dbe)?;
            f.write_all(data).await.map_err(dbe)?;
            f.sync_all().await.map_err(dbe)?;
        }
        tokio::fs::rename(&tmp, &path).await.map_err(dbe)?;
        // fsync the directory so the rename survives crashes.
        let d = tokio::fs::File::open(&dir).await.map_err(dbe)?;
        d.sync_all().await.map_err(dbe)?;
        Ok(true)
    }

    /// Synchronously writes a blob (used inside spawn_blocking contexts).
    pub fn write_blocking(&self, hash: &[u8; 32], data: &[u8]) -> Result<bool, StorageError> {
        let (dir, path) = self.shard_path(hash);
        if path.exists() {
            return Ok(false);
        }
        std::fs::create_dir_all(&dir).map_err(dbe)?;
        let tmp = dir.join(format!(".tmp-{}", Uuid::now_v7().as_simple()));
        {
            let mut f = std::fs::File::create(&tmp).map_err(dbe)?;
            f.write_all(data).map_err(dbe)?;
            f.sync_all().map_err(dbe)?;
        }
        std::fs::rename(&tmp, &path).map_err(dbe)?;
        let d = std::fs::File::open(&dir).map_err(dbe)?;
        d.sync_all().map_err(dbe)?;
        Ok(true)
    }

    /// Reads a blob into memory.
    pub async fn read(&self, hash: &[u8; 32]) -> Result<Bytes, StorageError> {
        let (_, path) = self.shard_path(hash);
        match tokio::fs::read(&path).await {
            Ok(v) => Ok(Bytes::from(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(not_found(format!("blob {}", hex::encode(hash))))
            }
            Err(e) => Err(dbe(e)),
        }
    }

    pub async fn exists(&self, hash: &[u8; 32]) -> Result<bool, StorageError> {
        let (_, path) = self.shard_path(hash);
        tokio::fs::try_exists(&path).await.map_err(dbe)
    }

    /// Blob size in bytes, if present.
    pub async fn size(&self, hash: &[u8; 32]) -> Result<Option<u64>, StorageError> {
        let (_, path) = self.shard_path(hash);
        match tokio::fs::metadata(&path).await {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(dbe(e)),
        }
    }

    /// File modification time (epoch ms), if present.
    pub async fn modified_ms(&self, hash: &[u8; 32]) -> Result<Option<i64>, StorageError> {
        let (_, path) = self.shard_path(hash);
        match tokio::fs::metadata(&path).await {
            Ok(m) => Ok(Some(
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(dbe(e)),
        }
    }

    /// Deletes a blob; returns true if it existed.
    pub async fn delete(&self, hash: &[u8; 32]) -> Result<bool, StorageError> {
        let (_, path) = self.shard_path(hash);
        match tokio::fs::remove_file(&path).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(dbe(e)),
        }
    }

    // -- GC two-phase delete barrier (regression W1-C-03 fix) ---------------
    //
    // The Wave-1 sweep re-checked references BEFORE the unlink, so an
    // upload could register a chunk in the gap between that re-check and
    // the unlink — the blob then vanished under a live row (dangling
    // manifest). The barrier makes the claim atomic: rename aside →
    // resurrection re-check → delete, or restore when an upload re-created
    // the row. The uploader's verified registration sees the canonical
    // path disappear and self-heals by rewriting (CAS: identical bytes).

    /// Atomically claims a blob for deletion by renaming it aside.
    /// Returns false when the blob is absent (or already claimed).
    pub async fn claim_for_gc(&self, hash: &[u8; 32]) -> Result<bool, StorageError> {
        let (dir, path) = self.shard_path(hash);
        let pending = dir.join(format!(
            "{}.{}.{pending_suffix}",
            hex::encode(hash),
            uuid::Uuid::now_v7().simple(),
            pending_suffix = Self::GC_PENDING_SUFFIX
        ));
        match tokio::fs::rename(&path, &pending).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(dbe(e)),
        }
    }

    /// Suffix for claimed blobs (never matched by `list_all`, which only
    /// accepts `<hex>.bin`).
    pub const GC_PENDING_SUFFIX: &str = "gc-pending";

    /// Restores a claimed blob (resurrection): rename back to canonical.
    /// When the canonical path was re-written meanwhile (uploader self-heal
    /// rewrites identical CAS bytes), the claimed copy is dropped instead.
    pub async fn restore_claimed(&self, hash: &[u8; 32]) -> Result<(), StorageError> {
        let (_, path) = self.shard_path(hash);
        if let Some(pending) = self.find_pending(hash).await? {
            if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                // Canonical re-written by a self-healing upload — same CAS
                // content; drop the stale claim.
                let _ = tokio::fs::remove_file(&pending).await;
            } else {
                let _ = tokio::fs::rename(&pending, &path).await;
            }
        }
        Ok(())
    }

    /// Deletes a claimed blob (the deletion is now final: no row exists).
    pub async fn delete_claimed(&self, hash: &[u8; 32]) -> Result<(), StorageError> {
        let (dir, _) = self.shard_path(hash);
        if let Some(pending) = self.find_pending(hash).await? {
            let _ = tokio::fs::remove_file(&pending).await;
        }
        let _ = dir;
        Ok(())
    }

    /// Finds a claimed blob in the shard dir (crash leftovers may leave
    /// several; the first is returned, boot sweep cleans stale ones).
    async fn find_pending(&self, hash: &[u8; 32]) -> Result<Option<PathBuf>, StorageError> {
        let (dir, _) = self.shard_path(hash);
        let prefix = format!("{}.", hex::encode(hash));
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(dbe(e)),
        };
        while let Some(e) = rd.next_entry().await.map_err(dbe)? {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && name.ends_with(Self::GC_PENDING_SUFFIX) {
                return Ok(Some(e.path()));
            }
        }
        Ok(None)
    }

    /// Lists every stored hash (used by the orphan sweeper). Walks the
    /// sharded tree to `shard_depth + 1` levels.
    pub async fn list_all(&self) -> Result<Vec<[u8; 32]>, StorageError> {
        let mut out = Vec::new();
        self.walk_dir(&self.root.clone(), self.shard_depth as usize + 1, &mut out)
            .await?;
        Ok(out)
    }

    fn walk_dir<'a>(
        &'a self,
        dir: &'a Path,
        depth: usize,
        out: &'a mut Vec<[u8; 32]>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>
    where
        Self: Send + Sync + 'a,
    {
        Box::pin(async move {
            if depth == 0 {
                return Ok(());
            }
            let mut entries = tokio::fs::read_dir(dir).await.map_err(dbe)?;
            while let Some(e) = entries.next_entry().await.map_err(dbe)? {
                let path = e.path();
                if path.is_dir() {
                    self.walk_dir(&path, depth - 1, out).await?;
                } else {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with(".tmp-") || name.starts_with(".write-probe-") {
                        continue;
                    }
                    let stem = name.strip_suffix(".bin").unwrap_or(&name);
                    if let Ok(h) = hex::decode(stem) {
                        if h.len() == 32 {
                            let mut arr = [0u8; 32];
                            arr.copy_from_slice(&h);
                            out.push(arr);
                        }
                    }
                }
            }
            Ok(())
        })
    }

    /// Total number of blobs and their combined byte size.
    pub async fn stats(&self) -> Result<(u64, u64), StorageError> {
        let hashes = self.list_all().await?;
        let mut total = 0u64;
        for h in &hashes {
            total += self.size(h).await?.unwrap_or(0);
        }
        Ok((hashes.len() as u64, total))
    }

    /// Ensures the root directory exists and is writable.
    pub async fn verify_writable(&self) -> Result<(), StorageError> {
        tokio::fs::create_dir_all(&self.root).await.map_err(dbe)?;
        let probe = self
            .root
            .join(format!(".write-probe-{}", Uuid::now_v7().as_simple()));
        tokio::fs::write(&probe, b"ok").await.map_err(dbe)?;
        tokio::fs::remove_file(&probe).await.map_err(dbe)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> (BlockStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (BlockStore::new(dir.path(), 2), dir)
    }

    #[tokio::test]
    async fn write_read_roundtrip() {
        let (bs, _dir) = store().await;
        let h = [9u8; 32];
        assert!(bs.write(&h, b"hello world").await.unwrap());
        assert_eq!(bs.read(&h).await.unwrap().as_ref(), b"hello world");
        assert!(bs.exists(&h).await.unwrap());
        assert_eq!(bs.size(&h).await.unwrap(), Some(11));
    }

    #[tokio::test]
    async fn write_is_idempotent() {
        let (bs, _dir) = store().await;
        let h = [1u8; 32];
        assert!(bs.write(&h, b"aaa").await.unwrap());
        assert!(!bs.write(&h, b"aaa").await.unwrap());
        assert_eq!(bs.read(&h).await.unwrap().as_ref(), b"aaa");
    }

    #[tokio::test]
    async fn sharded_layout() {
        let (bs, root) = store().await;
        let h: [u8; 32] = std::array::from_fn(|i| i as u8);
        bs.write(&h, b"x").await.unwrap();
        let hex = hex::encode(h);
        let expect = root.path().join("00").join("01").join(format!("{hex}.bin"));
        assert!(expect.exists(), "expected {}", expect.display());
        assert_eq!(bs.relative_path(&h), format!("00/01/{hex}.bin"));
    }

    #[tokio::test]
    async fn delete_and_missing() {
        let (bs, _dir) = store().await;
        let h = [2u8; 32];
        assert!(matches!(bs.read(&h).await, Err(StorageError::NotFound(_))));
        bs.write(&h, b"data").await.unwrap();
        assert!(bs.delete(&h).await.unwrap());
        assert!(!bs.delete(&h).await.unwrap());
    }

    #[tokio::test]
    async fn list_all_and_stats() {
        let (bs, _dir) = store().await;
        let mut hashes = Vec::new();
        for i in 0..5u8 {
            let mut h = [0u8; 32];
            h[31] = i;
            hashes.push(h);
            bs.write(&h, &[i; 100]).await.unwrap();
        }
        let listed = bs.list_all().await.unwrap();
        assert_eq!(listed.len(), 5);
        let (count, bytes) = bs.stats().await.unwrap();
        assert_eq!(count, 5);
        assert_eq!(bytes, 500);
        assert!(hashes.iter().all(|h| listed.contains(h)));
    }

    #[tokio::test]
    async fn shard_depth_zero_flat_layout() {
        let dir = tempfile::tempdir().unwrap();
        let bs = BlockStore::new(dir.path(), 0);
        let h = [5u8; 32];
        bs.write(&h, b"z").await.unwrap();
        let hex = hex::encode(h);
        assert!(dir.path().join(format!("{hex}.bin")).exists());
    }
}
