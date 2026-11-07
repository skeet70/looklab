// SPDX-License-Identifier: AGPL-3.0-or-later
//! Send manifest: the batch manifest hashes one passkey approval covers
//! ([docs/03 §2.3](../../../docs/03-data-model.md)).
//!
//! Canonical order is strictly increasing lexicographic order of the batch
//! manifest hash, independent of the client's batch enumeration.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::nonce::{ManifestHash, SendManifestHash};

pub const DEFAULT_MAX_BATCHES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendManifest {
    pub batches: Vec<ManifestHash>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendManifestError {
    #[error("a send manifest lists no batch")]
    Empty,
    #[error("a send manifest lists {count} batches, outside 1..={max}")]
    TooMany { count: usize, max: usize },
    #[error("send manifest entry {index} repeats an earlier batch hash")]
    Duplicate { index: usize },
    #[error("send manifest entry {index} is not in canonical order")]
    NotSorted { index: usize },
}

impl SendManifest {
    /// Sorts the hashes into canonical order.
    pub fn canonical(batches: Vec<ManifestHash>) -> Result<Self, SendManifestError> {
        let mut batches = batches;
        batches.sort_unstable();
        let manifest = Self { batches };
        manifest.validate(usize::MAX)?;
        Ok(manifest)
    }

    pub fn validate(&self, max_batches: usize) -> Result<(), SendManifestError> {
        if self.batches.is_empty() {
            return Err(SendManifestError::Empty);
        }
        if self.batches.len() > max_batches {
            return Err(SendManifestError::TooMany {
                count: self.batches.len(),
                max: max_batches,
            });
        }
        for (index, pair) in self.batches.windows(2).enumerate() {
            if pair[0] == pair[1] {
                return Err(SendManifestError::Duplicate { index: index + 1 });
            }
            if pair[0] > pair[1] {
                return Err(SendManifestError::NotSorted { index: index + 1 });
            }
        }
        Ok(())
    }

    pub fn contains(&self, hash: &ManifestHash) -> bool {
        self.batches.binary_search(hash).is_ok()
    }
}

/// The bytes whose SHA-256 is the send manifest hash and which the envelope carries.
pub fn canonical_send_manifest_bytes(manifest: &SendManifest) -> Vec<u8> {
    postcard::to_allocvec(manifest).expect("a send manifest has no unserializable value")
}

pub fn send_manifest_hash(manifest: &SendManifest) -> SendManifestHash {
    let digest = Sha256::digest(canonical_send_manifest_bytes(manifest));
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    SendManifestHash::from_bytes(bytes)
}
