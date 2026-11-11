// SPDX-License-Identifier: AGPL-3.0-or-later
//! Content-addressed blob storage.

use std::path::PathBuf;

use sha2::{Digest, Sha256};

static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

fn write_temporary(path: &std::path::Path, bytes: &[u8]) -> Result<(), BlobError> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn verify_existing(path: &std::path::Path, hash: BlobHash, bytes: &[u8]) -> Result<(), BlobError> {
    let existing = std::fs::read(path)?;
    if BlobHash::of(&existing) != hash || existing != bytes {
        return Err(BlobError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "existing blob content does not match its hash",
        )));
    }
    Ok(())
}

/// Writes under a transaction the blocking task owns.
pub async fn write_guarded<T, F>(
    tx: sqlx::Transaction<'static, sqlx::Sqlite>,
    write: F,
) -> Result<(sqlx::Transaction<'static, sqlx::Sqlite>, T), BlobError>
where
    T: Send + 'static,
    F: FnOnce() -> (T, Result<(), BlobError>) + Send + 'static,
{
    let (tx, output, written) = tokio::task::spawn_blocking(move || {
        let (output, written) = write();
        (tx, output, written)
    })
    .await
    .map_err(|error| BlobError::Io(std::io::Error::other(error)))?;
    written?;
    Ok((tx, output))
}

impl BlobStore for FsBlobStore {
    fn put(&self, hash: BlobHash, bytes: &[u8]) -> Result<(), BlobError> {
        let path = self.path(hash);
        if path.exists() {
            return verify_existing(&path, hash, bytes);
        }
        let unique = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary = self.root.join(format!(
            ".{}.{}.{}.tmp",
            hash.to_hex(),
            std::process::id(),
            unique
        ));
        let result = write_temporary(&temporary, bytes).and_then(|()| {
            match std::fs::rename(&temporary, &path) {
                Ok(()) => Ok(()),
                Err(_) if path.exists() => verify_existing(&path, hash, bytes),
                Err(error) => Err(error.into()),
            }
        });
        let _ = std::fs::remove_file(&temporary);
        result?;
        if let Ok(directory) = std::fs::File::open(&self.root) {
            let _ = directory.sync_all();
        }
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::write_guarded;

    struct Release(std::sync::mpsc::Sender<()>);

    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_writer_holds_the_transaction_until_the_write_returns() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("looklab.sqlite3");
        let pool = crate::db::open(&path).await.expect("database");
        let tx = pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .expect("transaction");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_guard = Release(release_tx.clone());
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(write_guarded(tx, move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
            let _ = finished_tx.send(());
            ((), Ok(()))
        }));
        tokio::time::timeout(Duration::from_secs(5), entered_rx)
            .await
            .expect("writer entered")
            .expect("entered signal");
        task.abort();
        let _ = task.await;

        let contender = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .busy_timeout(Duration::from_millis(100)),
        )
        .await
        .expect("contender");
        let locked = contender.begin_with("BEGIN IMMEDIATE").await.is_err();
        release_tx.send(()).expect("release writer");
        assert!(
            locked,
            "a cancelled request must not release the write lock while the writer runs"
        );
        tokio::time::timeout(Duration::from_secs(5), finished_rx)
            .await
            .expect("writer finished")
            .expect("finished signal");
        drop(release_guard);
        contender.close().await;
        pool.close().await;
    }
}
