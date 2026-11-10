//! Per-tenant AES-256-GCM encryption at rest.
//!
//! Blob layout on disk: `nonce(12) || ciphertext || tag(16)`.
//! Keys are 32 random bytes per tenant, stored hex-encoded in
//! `<keys_dir>/<tenant>.key` with `0600` permissions.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{dbe, StorageError};
use crate::storage::index_db::now_ms;
use uuid::Uuid;

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

pub struct Encryptor {
    keys_dir: PathBuf,
    cache: DashMap<Uuid, Arc<[u8; 32]>>,
}

impl Encryptor {
    pub fn new(keys_dir: PathBuf) -> Self {
        Self {
            keys_dir,
            cache: DashMap::new(),
        }
    }

    fn key_path(&self, tenant: &Uuid) -> PathBuf {
        self.keys_dir.join(format!("{}.key", tenant.simple()))
    }

    /// Loads (or creates) the tenant's 32-byte key.
    pub fn key_for(&self, tenant: &Uuid) -> Result<Arc<[u8; 32]>, StorageError> {
        if let Some(k) = self.cache.get(tenant) {
            return Ok(k.clone());
        }
        std::fs::create_dir_all(&self.keys_dir).map_err(dbe)?;
        let path = self.key_path(tenant);
        let key: [u8; 32] = if path.exists() {
            let text = std::fs::read_to_string(&path).map_err(dbe)?;
            let bytes = hex::decode(text.trim()).map_err(|e| {
                StorageError::Encryption(format!("key file {path:?} invalid hex: {e}"))
            })?;
            bytes.try_into().map_err(|_| {
                StorageError::Encryption(format!("key file {path:?} must be 32 bytes"))
            })?
        } else {
            let key: [u8; 32] = rand::random();
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(dbe)?;
            f.write_all(hex::encode(key).as_bytes()).map_err(dbe)?;
            f.sync_all().map_err(dbe)?;
            key
        };
        let key = Arc::new(key);
        self.cache.insert(*tenant, key.clone());
        Ok(key)
    }

    fn cipher(key: &[u8; 32]) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(key).expect("32-byte key is valid for AES-256")
    }

    /// Encrypts `plaintext`; output is `nonce || ciphertext || tag`.
    pub fn encrypt(&self, tenant: &Uuid, plaintext: &[u8]) -> Result<Vec<u8>, StorageError> {
        let key = self.key_for(tenant)?;
        let cipher = Self::cipher(&key);
        let nonce_bytes: [u8; NONCE_LEN] = rand::random();
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| StorageError::Encryption(format!("encrypt: {e}")))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Decrypts a `nonce || ciphertext || tag` blob.
    pub fn decrypt(&self, tenant: &Uuid, blob: &[u8]) -> Result<Vec<u8>, StorageError> {
        if blob.len() < NONCE_LEN + TAG_LEN {
            return Err(StorageError::Encryption(format!(
                "encrypted blob too short ({} bytes)",
                blob.len()
            )));
        }
        let key = self.key_for(tenant)?;
        let cipher = Self::cipher(&key);
        let nonce = Nonce::from_slice(&blob[..NONCE_LEN]);
        cipher.decrypt(nonce, &blob[NONCE_LEN..]).map_err(|_| {
            StorageError::Encryption("decryption failed (wrong key or tampered data)".into())
        })
    }

    /// True when the key file for `tenant` exists on disk.
    pub fn has_key(&self, tenant: &Uuid) -> bool {
        self.key_path(tenant).exists()
    }

    /// Storage key for a plaintext chunk hash under a tenant:
    /// `BLAKE3(tenant_id || plain_hash)` — keeps encrypted dedup scoped to
    /// the tenant (random nonces make ciphertext-addressing impossible).
    pub fn storage_key(tenant: &Uuid, plain_hash: &[u8; 32]) -> [u8; 32] {
        let mut buf = Vec::with_capacity(16 + 32);
        buf.extend_from_slice(tenant.as_bytes());
        buf.extend_from_slice(plain_hash);
        crate::core::hasher::hash_bytes(&buf)
    }

    /// Last modification of a tenant key file (epoch ms) — diagnostics.
    pub fn key_age_ms(&self, tenant: &Uuid) -> Option<i64> {
        let meta = std::fs::metadata(self.key_path(tenant)).ok()?;
        let modified = meta.modified().ok()?;
        let age = modified
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .ok()?;
        Some(age.as_millis() as i64 - now_ms())
    }
}

/// Verifies the on-disk key file permissions are owner-only (unix).
#[cfg(unix)]
pub fn key_file_permissions_ok(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) => m.permissions().mode() & 0o077 == 0,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encryptor() -> (Encryptor, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Encryptor::new(dir.path().to_path_buf()), dir)
    }

    #[test]
    fn roundtrip() {
        let (enc, _d) = encryptor();
        let tenant = Uuid::now_v7();
        let data = b"attack at dawn \xF0\x9F\x9A\x80";
        let ct = enc.encrypt(&tenant, data).unwrap();
        assert_eq!(ct.len(), data.len() + NONCE_LEN + TAG_LEN);
        let pt = enc.decrypt(&tenant, &ct).unwrap();
        assert_eq!(pt, data);
    }

    #[test]
    fn nonce_uniqueness() {
        let (enc, _d) = encryptor();
        let tenant = Uuid::now_v7();
        let a = enc.encrypt(&tenant, b"same").unwrap();
        let b = enc.encrypt(&tenant, b"same").unwrap();
        assert_ne!(a, b, "random nonces must produce distinct ciphertexts");
    }

    #[test]
    fn tamper_detected() {
        let (enc, _d) = encryptor();
        let tenant = Uuid::now_v7();
        let mut ct = enc.encrypt(&tenant, b"important").unwrap();
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        assert!(enc.decrypt(&tenant, &ct).is_err());
    }

    #[test]
    fn wrong_key_rejected() {
        let (enc, _d) = encryptor();
        let t1 = Uuid::now_v7();
        let t2 = Uuid::now_v7();
        let ct = enc.encrypt(&t1, b"secret").unwrap();
        assert!(enc.decrypt(&t2, &ct).is_err());
    }

    #[test]
    fn key_file_persisted_with_0600() {
        let (enc, dir) = encryptor();
        let tenant = Uuid::now_v7();
        let k1 = enc.key_for(&tenant).unwrap();
        let path = dir.path().join(format!("{}.key", tenant.simple()));
        assert!(path.exists());
        #[cfg(unix)]
        assert!(key_file_permissions_ok(&path), "key file must be 0600");
        // reload from disk
        let enc2 = Encryptor::new(dir.path().to_path_buf());
        let k2 = enc2.key_for(&tenant).unwrap();
        assert_eq!(*k1, *k2);
        assert!(enc.has_key(&tenant));
    }

    #[test]
    fn storage_key_is_tenant_scoped() {
        let t1 = Uuid::now_v7();
        let t2 = Uuid::now_v7();
        let h = [9u8; 32];
        assert_ne!(
            Encryptor::storage_key(&t1, &h),
            Encryptor::storage_key(&t2, &h)
        );
        assert_eq!(
            Encryptor::storage_key(&t1, &h),
            Encryptor::storage_key(&t1, &h)
        );
    }

    #[test]
    fn short_blob_rejected() {
        let (enc, _d) = encryptor();
        let tenant = Uuid::now_v7();
        assert!(enc.decrypt(&tenant, b"short").is_err());
        assert!(enc.decrypt(&tenant, &[0u8; 20]).is_err());
    }
}
