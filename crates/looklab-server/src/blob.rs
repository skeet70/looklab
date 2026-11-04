// SPDX-License-Identifier: AGPL-3.0-or-later
//! Content-addressed blob storage.

use std::path::PathBuf;

use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("blob storage: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlobHash([u8; 32]);

impl BlobHash {
    pub fn of(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&digest);
        Self(hash)
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 64 {
            return None;
        }
        let mut hash = [0u8; 32];
        for index in 0..32 {
            let high = hex_nibble(bytes[index * 2])?;
            let low = hex_nibble(bytes[index * 2 + 1])?;
            hash[index] = (high << 4) | low;
        }
        Some(Self(hash))
    }
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl core::fmt::Debug for BlobHash {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

pub trait BlobStore: Send + Sync {
    fn put(&self, hash: BlobHash, bytes: &[u8]) -> Result<(), BlobError>;
    fn get(&self, hash: BlobHash) -> Result<Option<Vec<u8>>, BlobError>;
    fn delete(&self, hash: BlobHash) -> Result<(), BlobError>;
}

/// Blobs are stored as `<hex sha256>` directly under `root`.
#[derive(Debug, Clone)]
pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, BlobError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { root })
    }

    fn path(&self, hash: BlobHash) -> PathBuf {
        self.root.join(hash.to_hex())
    }
}

impl BlobStore for FsBlobStore {
    fn put(&self, hash: BlobHash, bytes: &[u8]) -> Result<(), BlobError> {
        let path = self.path(hash);
        if path.exists() {
            return Ok(());
        }
        let temporary = self.root.join(format!(".{}.tmp", hash.to_hex()));
        std::fs::write(&temporary, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&temporary, &path)?;
        Ok(())
    }

    fn get(&self, hash: BlobHash) -> Result<Option<Vec<u8>>, BlobError> {
        match std::fs::read(self.path(hash)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn delete(&self, hash: BlobHash) -> Result<(), BlobError> {
        match std::fs::remove_file(self.path(hash)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
